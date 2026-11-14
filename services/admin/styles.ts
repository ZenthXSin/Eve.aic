/**
 * 管理面板样式。全部经 nonce CSP 内联，不使用 style 属性、外部字体或图片。
 * 颜色只通过下方变量取用；明暗主题由 <html data-theme> 切换，脚本未运行前跟随系统。
 */
const light = `
  color-scheme: light;
  --bg: #f4f6fa; --surface: #ffffff; --raised: #f7f9fc; --sunken: #edf0f5;
  --text: #16202d; --muted: #5a6577; --faint: #8a94a4;
  --line: #e3e8ef; --line-strong: #cdd5df;
  --accent: #0c9a74; --accent-ink: #ffffff; --accent-soft: #e0f6ee; --accent-line: #9bdcc4;
  --warn: #9a6208; --warn-soft: #fff4dc; --warn-line: #f1d9a0;
  --danger: #c73a35; --danger-soft: #fdeceb; --danger-line: #f2c0bd;
  --info: #2f6fed; --info-soft: #e8f0ff; --violet: #7a63f2;
  --chart-rest: #dbe2ec; --chart-unknown: #f0b44c;
  --shadow: 0 1px 2px #1620300a, 0 8px 24px #1620300d;
  --shadow-pop: 0 16px 48px #16203030;
`;
const dark = `
  color-scheme: dark;
  --bg: #0d1117; --surface: #151b23; --raised: #1b222c; --sunken: #10151c;
  --text: #e6ebf2; --muted: #9aa5b5; --faint: #6b7686;
  --line: #252d38; --line-strong: #374252;
  --accent: #3fd0a4; --accent-ink: #06241a; --accent-soft: #12362c; --accent-line: #236650;
  --warn: #f0c067; --warn-soft: #3a2e15; --warn-line: #5e4b22;
  --danger: #ff9a8f; --danger-soft: #40201d; --danger-line: #6e3531;
  --info: #8fb4ff; --info-soft: #17284a; --violet: #a99bff;
  --chart-rest: #2f3948; --chart-unknown: #d9a441;
  --shadow: 0 1px 2px #0006, 0 8px 24px #0005;
  --shadow-pop: 0 16px 48px #000b;
`;
/** 各应用的识别色：星图节点、主屏图标、使用环与图例共用。 */
const appColors = `
[data-app=qq] { --app: #2f7df6; --app-2: #63a4ff; }
[data-app=files] { --app: #f08c1a; --app-2: #fbbf4d; }
[data-app=terminal] { --app: #3d4a5c; --app-2: #6b7a90; }
[data-app=notes] { --app: #e0a800; --app-2: #f7d24a; }
[data-app=memory] { --app: #7a63f2; --app-2: #a996ff; }
[data-app=browser] { --app: #0aa5c2; --app-2: #45d0e6; }
[data-app=tools] { --app: #e2478b; --app-2: #f783b4; }
`;

export const panelCss = `
:root { ${light}
  --radius: 14px; --radius-sm: 9px; --nav-width: 236px; --tabbar: 58px;
  --mono: ui-monospace, SFMono-Regular, "JetBrains Mono", Consolas, monospace;
  font-family: Inter, "PingFang SC", "HarmonyOS Sans SC", "Microsoft YaHei", system-ui, sans-serif;
  font-synthesis: none; background: var(--bg); color: var(--text);
}
@media (prefers-color-scheme: dark) { :root:not([data-theme=light]) { ${dark} } }
:root[data-theme=dark] { ${dark} }

/* ── 基础 ─────────────────────────────────────────── */
* { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; }
body { margin: 0; font-size: 14px; line-height: 1.6; background: var(--bg); color: var(--text); overflow-wrap: anywhere; }
h1, h2, h3, h4, p, dl, dd { margin: 0; }
h1 { font-size: 20px; font-weight: 650; letter-spacing: -.2px; }
h2 { font-size: 16px; font-weight: 650; }
h3 { font-size: 14px; font-weight: 600; }
small, .muted { color: var(--muted); }
[hidden] { display: none !important; }
svg.icon { width: 20px; height: 20px; flex: none; fill: none; stroke: currentColor; stroke-width: 1.7; stroke-linecap: round; stroke-linejoin: round; }
button, input, select { font: inherit; color: inherit; }
button {
  display: inline-flex; align-items: center; justify-content: center; gap: 7px; min-height: 38px; padding: 7px 14px;
  border: 1px solid var(--line-strong); border-radius: var(--radius-sm); background: var(--surface); cursor: pointer;
  font-weight: 500; transition: background .15s, border-color .15s, opacity .2s .3s;
}
button:hover { background: var(--raised); border-color: var(--accent-line); }
button:active { transform: translateY(1px); }
/* 请求进行时全部按钮短暂禁用；延迟变淡，避免每次同步都闪烁。 */
button:disabled { cursor: default; opacity: .55; }
button.primary { background: var(--accent); border-color: var(--accent); color: var(--accent-ink); }
button.primary:hover { filter: brightness(1.08); }
button.danger { color: var(--danger); border-color: var(--danger-line); }
button.danger:hover { background: var(--danger-soft); }
button.quiet { border-color: transparent; background: transparent; color: var(--muted); }
button.quiet:hover { background: var(--raised); color: var(--text); }
button.link, button.stream-original, button.section-link, button.workspace-original {
  min-height: 0; padding: 2px 4px; border: 0; background: transparent; color: var(--accent); font-size: 12px; font-weight: 500;
}
button.link:hover, button.stream-original:hover, button.section-link:hover, button.workspace-original:hover { text-decoration: underline; background: transparent; }
button.small { min-height: 30px; padding: 4px 10px; font-size: 12px; }
:is(button, input, select, summary, a, [tabindex]):focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
input, select {
  min-height: 38px; min-width: 0; max-width: 100%; padding: 7px 11px; border: 1px solid var(--line-strong);
  border-radius: var(--radius-sm); background: var(--surface);
}
input::placeholder { color: var(--faint); }
summary { cursor: pointer; color: var(--muted); font-size: 12px; }
pre {
  margin: 10px 0 0; padding: 12px 14px; max-height: 420px; overflow: auto; white-space: pre-wrap;
  font: 12px/1.7 var(--mono); background: var(--sunken); border: 1px solid var(--line); border-radius: var(--radius-sm);
}
.skip { position: absolute; left: -9999px; }
.skip:focus { left: 16px; top: 10px; z-index: 50; padding: 8px 12px; background: var(--surface); border-radius: 8px; }
.row { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.hint { color: var(--muted); font-size: 12px; line-height: 1.7; }
.panel { background: var(--surface); border: 1px solid var(--line); border-radius: var(--radius); padding: 20px; box-shadow: var(--shadow); min-width: 0; }
.panel-title { display: flex; justify-content: space-between; align-items: flex-start; gap: 12px; margin-bottom: 14px; }
.badge {
  display: inline-flex; align-items: center; gap: 6px; padding: 3px 10px; border-radius: 999px; white-space: nowrap;
  font-size: 12px; font-weight: 550; background: var(--accent-soft); color: var(--accent);
}
.badge::before { content: ""; width: 7px; height: 7px; border-radius: 50%; background: currentColor; }
.badge.warn { background: var(--warn-soft); color: var(--warn); }
.badge.error { background: var(--danger-soft); color: var(--danger); }
.notice { padding: 12px 14px; border: 1px solid var(--warn-line); border-radius: var(--radius-sm); background: var(--warn-soft); color: var(--warn); }
.notice p { margin-bottom: 10px; }
.empty { padding: 44px 20px; text-align: center; color: var(--muted); border: 1px dashed var(--line-strong); border-radius: var(--radius); }
.empty strong { display: block; margin-bottom: 4px; color: var(--text); font-size: 15px; }
.empty-mark { font-size: 26px; color: var(--faint); margin-bottom: 8px; }
.empty.error { border-color: var(--danger-line); }
.segmented { display: inline-flex; gap: 2px; padding: 3px; background: var(--sunken); border: 1px solid var(--line); border-radius: 11px; max-width: 100%; overflow-x: auto; scrollbar-width: none; }
.segmented button { min-height: 32px; padding: 4px 13px; border: 0; background: transparent; color: var(--muted); font-size: 13px; border-radius: 8px; white-space: nowrap; }
.segmented button:is([aria-pressed=true], [aria-current=page]) { background: var(--surface); color: var(--text); box-shadow: var(--shadow); font-weight: 600; }
.count { min-width: 18px; padding: 0 5px; border-radius: 9px; background: var(--danger); color: #fff; font-size: 11px; line-height: 18px; text-align: center; font-weight: 600; }

/* ── 登录 ─────────────────────────────────────────── */
.login-wrap { position: relative; min-height: 100dvh; display: grid; place-items: center; padding: 20px; overflow: hidden; }
.login-wrap::before, .login-wrap::after { content: ""; position: absolute; width: 60vmax; height: 60vmax; border-radius: 50%; filter: blur(80px); opacity: .5; animation: float 16s ease-in-out infinite alternate; }
.login-wrap::before { top: -25vmax; left: -15vmax; background: color-mix(in srgb, var(--accent) 45%, transparent); }
.login-wrap::after { bottom: -30vmax; right: -15vmax; background: color-mix(in srgb, var(--info) 40%, transparent); animation-delay: -8s; }
@keyframes float { to { transform: translate(8vmax, 6vmax) scale(1.15); } }
.login-card { position: relative; z-index: 1; }
.login-card { width: min(400px, 100%); padding: 32px; background: var(--surface); border: 1px solid var(--line); border-radius: 20px; box-shadow: var(--shadow-pop); }
.login-card h1 { margin: 22px 0 4px; font-size: 22px; }
.login-card p { color: var(--muted); }
.login-card form { display: grid; gap: 8px; margin-top: 22px; }
.login-card label { font-size: 13px; font-weight: 550; }
.login-card input { width: 100%; min-height: 44px; font-size: 16px; }
.login-card form button { min-height: 44px; margin-top: 8px; }
#loginMessage { margin-top: 12px; color: var(--danger); min-height: 1.6em; }
.brand { display: flex; align-items: center; gap: 10px; }
.mark { display: grid; place-items: center; width: 34px; height: 34px; flex: none; border-radius: 11px; background: var(--accent); color: var(--accent-ink); font: 600 22px/1 Georgia, serif; }
.brand strong { display: block; font-size: 16px; line-height: 1.2; }
.brand small { font-size: 11px; }

/* ── 外壳：桌面侧栏 / 手机顶栏与底部导航 ─────────── */
.shell { display: grid; grid-template-columns: var(--nav-width) minmax(0, 1fr); min-height: 100dvh; }
.sidebar { position: sticky; top: 0; height: 100dvh; display: flex; flex-direction: column; gap: 18px; padding: 20px 14px; background: var(--surface); border-right: 1px solid var(--line); overflow-y: auto; }
.sidebar .brand { padding: 0 8px; }
.sidebar nav { display: grid; gap: 3px; }
.nav-button { justify-content: flex-start; gap: 11px; width: 100%; min-height: 42px; padding: 8px 12px; border: 0; background: transparent; color: var(--muted); border-radius: 10px; }
.nav-button:hover { background: var(--raised); color: var(--text); }
.nav-button[aria-current=page] { background: var(--accent-soft); color: var(--accent); font-weight: 650; }
.nav-button .count { margin-left: auto; }
.side-bottom { margin-top: auto; display: grid; gap: 3px; }
.main { min-width: 0; width: 100%; max-width: 1560px; margin: 0 auto; padding: 0 28px 32px; }
.topbar { position: sticky; top: 0; z-index: 20; display: flex; align-items: center; justify-content: space-between; gap: 12px; margin: 0 -28px 20px; padding: 14px 28px; background: color-mix(in srgb, var(--bg) 88%, transparent); backdrop-filter: blur(10px); border-bottom: 1px solid transparent; }
.topbar .mark { display: none; }
.topbar-title { display: flex; align-items: center; gap: 10px; min-width: 0; }
.topbar-title p { font-size: 12px; color: var(--muted); }
.top-actions { display: flex; align-items: center; gap: 8px; }
.sync { font-size: 12px; color: var(--faint); white-space: nowrap; }
.icon-button { width: 38px; padding: 0; }
.main[aria-busy=true] .icon-button#refresh svg { animation: spin .9s linear infinite; }
.tabbar { display: none; }
.toast { position: fixed; left: 50%; bottom: 24px; z-index: 60; transform: translateX(-50%); width: max-content; max-width: min(560px, calc(100vw - 32px)); padding: 11px 16px; border-radius: 12px; background: var(--text); color: var(--bg); box-shadow: var(--shadow-pop); font-size: 13px; animation: rise .2s ease-out; }
.toast.error { background: var(--danger); color: #fff; }
:root[data-theme=dark] .toast.error { color: #2a0f0a; }
.view { animation: fade .18s ease-out; }
.view-head { display: flex; align-items: flex-end; justify-content: space-between; gap: 12px 20px; flex-wrap: wrap; margin-bottom: 16px; }
.view-head p { color: var(--muted); max-width: 720px; font-size: 13px; }
@keyframes spin { to { transform: rotate(360deg); } }
@keyframes rise { from { opacity: 0; transform: translate(-50%, 8px); } }
@keyframes fade { from { opacity: .4; } }
@keyframes enter { from { opacity: .3; transform: translateY(5px); } }

/* ── 总览 ─────────────────────────────────────────── */
.hero { display: flex; align-items: center; justify-content: space-between; gap: 16px 24px; flex-wrap: wrap; padding: 22px 24px; margin-bottom: 16px; border-radius: var(--radius); border: 1px solid var(--accent-line); background: linear-gradient(135deg, var(--accent-soft), var(--surface) 75%); }
.hero[data-tone=warn] { border-color: var(--warn-line); background: linear-gradient(135deg, var(--warn-soft), var(--surface) 75%); }
.hero[data-tone=error] { border-color: var(--danger-line); background: linear-gradient(135deg, var(--danger-soft), var(--surface) 75%); }
.hero h2 { font-size: 21px; letter-spacing: -.3px; margin-bottom: 3px; }
.hero p { color: var(--muted); max-width: 640px; }
.attention { display: grid; gap: 8px; margin-bottom: 16px; }
.attention-item { display: flex; align-items: center; gap: 12px; padding: 10px 14px; border: 1px solid var(--warn-line); border-left-width: 4px; border-radius: var(--radius-sm); background: var(--surface); }
.attention-item[data-tone=error] { border-color: var(--danger-line); }
.attention-item span { flex: 1; min-width: 0; }
.attention-ok { color: var(--muted); font-size: 13px; padding: 2px 4px; }
.tiles { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 12px; margin-bottom: 16px; }
.tile { padding: 16px 18px; background: var(--surface); border: 1px solid var(--line); border-radius: var(--radius); box-shadow: var(--shadow); min-width: 0; }
.tile, .panel, .record, .attention-item { animation: enter .35s ease-out backwards; }
.tile:nth-child(2) { animation-delay: .05s; } .tile:nth-child(3) { animation-delay: .1s; } .tile:nth-child(4) { animation-delay: .15s; }
.tile .label { font-size: 12px; color: var(--muted); }
.tile .number { margin: 4px 0 2px; font-size: 24px; font-weight: 650; letter-spacing: -.5px; line-height: 1.3; font-variant-numeric: tabular-nums; }
.tile .number.text { font-size: 17px; letter-spacing: 0; }
.tile small { font-size: 12px; }
.meter { display: block; width: 100%; height: 6px; margin: 8px 0 6px; border-radius: 3px; background: var(--sunken); }
.meter-known { fill: var(--accent); }
.meter-reserved { fill: var(--chart-unknown); }
.grid-two { display: grid; grid-template-columns: minmax(0, 1.5fr) minmax(0, 1fr); gap: 16px; align-items: start; }
.stack { display: grid; gap: 16px; min-width: 0; }
.usage-summary { display: flex; gap: 10px 32px; flex-wrap: wrap; margin-bottom: 12px; }
.usage-stat strong { display: block; font-size: 22px; font-weight: 650; letter-spacing: -.4px; font-variant-numeric: tabular-nums; }
.usage-stat span { font-size: 12px; color: var(--muted); }
.chart-insight { padding: 9px 13px; margin-bottom: 12px; border-left: 3px solid var(--accent-line); background: var(--raised); border-radius: 0 8px 8px 0; font-size: 13px; }
.chart-legend { display: flex; gap: 6px 16px; flex-wrap: wrap; font-size: 12px; color: var(--muted); }
.dot { display: inline-block; width: 9px; height: 9px; margin-right: 6px; border-radius: 3px; background: var(--accent); }
.dot.remaining { background: var(--chart-rest); }
.dot.unknown, .dot.reserved { background: var(--chart-unknown); }
.chart-scroll { overflow-x: auto; }
.usage-chart { display: block; width: 100%; min-width: 560px; height: auto; margin-top: 6px; }
/* 图形由脚本以固定色绘制；此处按主题重映射。 */
.usage-chart line { stroke: var(--line); }
.usage-chart line[stroke-dasharray] { stroke: var(--chart-unknown); }
.usage-chart text { fill: var(--faint); }
.usage-chart rect[fill="#dfe6da"] { fill: var(--chart-rest); }
.usage-chart rect[fill="#d0b67c"] { fill: var(--chart-unknown); }
.usage-chart rect[fill="#32684e"] { fill: var(--accent); }
.usage-chart g { cursor: pointer; outline: none; }
.usage-chart g rect { transform-box: fill-box; transform-origin: bottom; animation: grow .7s cubic-bezier(.3, .7, .2, 1) backwards; }
@keyframes grow { from { transform: scaleY(0); } }
.usage-chart g:is(:focus, :hover) rect { stroke: var(--text); stroke-width: 1.5; }
.chart-readout { min-height: 40px; padding: 9px 13px; margin-top: 6px; border-radius: 8px; background: var(--raised); font-size: 12px; color: var(--muted); }
.chart-empty { padding: 32px; text-align: center; color: var(--muted); }
.chart-table { overflow: auto; }
.chart-data { width: 100%; border-collapse: collapse; font-size: 12px; }
.chart-data :is(th, td) { padding: 7px 6px; text-align: left; border-bottom: 1px solid var(--line); font-variant-numeric: tabular-nums; }
.usage-panel details { margin-top: 12px; }
.budget-viz { display: flex; align-items: center; gap: 22px; }
.ring { position: relative; width: 132px; flex: none; }
.ring svg { display: block; width: 100%; transform: rotate(-90deg); }
.ring circle { fill: none; stroke-width: 11; }
.ring-known, .ring-reserved { transition: stroke-dasharray .9s cubic-bezier(.3, .7, .2, 1), stroke-dashoffset .9s cubic-bezier(.3, .7, .2, 1); }
.ring-base { stroke: var(--sunken); }
.ring-known { stroke: var(--accent); }
.ring-reserved { stroke: var(--chart-unknown); }
.ring-label { position: absolute; inset: 0; display: grid; place-content: center; text-align: center; }
.ring-label strong { font-size: 24px; font-weight: 650; line-height: 1.2; }
.ring-label small { font-size: 11px; }
.legend { flex: 1; min-width: 0; display: grid; gap: 9px; }
.legend-row { display: flex; justify-content: space-between; gap: 10px; font-size: 13px; }
.legend-row strong { font-weight: 600; font-variant-numeric: tabular-nums; }
.facts { display: grid; }
.facts > div { display: flex; justify-content: space-between; gap: 14px; padding: 10px 0; border-bottom: 1px solid var(--line); font-size: 13px; }
.facts > div:last-child { border: 0; }
.facts span { color: var(--muted); }
.facts strong { font-weight: 550; text-align: right; }

/* ── 记录 ─────────────────────────────────────────── */
.searchbar { display: flex; gap: 8px; flex-wrap: wrap; margin: 14px 0 10px; }
.searchbar input { flex: 1 1 200px; }
.chips { display: flex; gap: 6px; flex-wrap: wrap; margin-bottom: 10px; }
.chips button { min-height: 30px; padding: 3px 12px; border-radius: 999px; font-size: 12px; color: var(--muted); }
.chips button[aria-pressed=true] { background: var(--accent-soft); border-color: var(--accent-line); color: var(--accent); font-weight: 600; }
.items { display: grid; gap: 10px; margin-top: 10px; }
.record { padding: 14px 16px; background: var(--surface); border: 1px solid var(--line); border-left: 4px solid var(--line-strong); border-radius: var(--radius-sm); min-width: 0; }
.record[data-category=thought] { border-left-color: var(--accent); }
.record[data-category=message] { border-left-color: var(--info); }
.record[data-category=tool] { border-left-color: var(--chart-unknown); }
.record[data-category=error], .record[data-resource=qqFailures] { border-left-color: var(--danger); }
.item-head { display: flex; align-items: flex-start; justify-content: space-between; gap: 12px; }
.item-title { font-weight: 600; }
.timeline-kind { display: none; }
.meta { display: flex; gap: 3px 12px; flex-wrap: wrap; margin-top: 2px; color: var(--faint); font-size: 12px; }
.timeline-version { font-family: var(--mono); }
.record-actions { display: flex; gap: 6px; flex: none; }
.record-actions button { min-height: 30px; padding: 4px 10px; font-size: 12px; }
.record-body { margin-top: 8px; max-height: 190px; overflow: hidden; white-space: pre-wrap; color: var(--muted); font-size: 13px; line-height: 1.75; mask-image: linear-gradient(#000 150px, transparent); }
.record[data-category=thought] .record-body { color: var(--text); }
.record:is([data-category=model], [data-category=system], [data-category=tool], [data-category=context], [data-category=rest], [data-category=upgrade]) .record-body { max-height: 76px; mask-image: linear-gradient(#000 44px, transparent); font: 12px/1.6 var(--mono); }
.record details { margin-top: 8px; }
.timeline-links { display: flex; gap: 4px 10px; flex-wrap: wrap; margin-top: 8px; }
.page-end { display: flex; justify-content: center; margin: 18px 0 0; }

/* ── 阅读抽屉与确认框 ─────────────────────────────── */
dialog { color: var(--text); background: var(--surface); border: 1px solid var(--line); box-shadow: var(--shadow-pop); padding: 0; }
dialog::backdrop { background: #0b141080; }
.drawer { margin: 0 0 0 auto; width: min(620px, 100vw); height: 100dvh; max-height: none; border-width: 0 0 0 1px; }
.drawer[open] { display: flex; flex-direction: column; animation: slide .2s ease-out; }
@keyframes slide { from { transform: translateX(40px); opacity: .5; } }
.dialog-head { display: flex; align-items: center; gap: 8px; padding: 14px 18px; border-bottom: 1px solid var(--line); }
.dialog-head h2 { margin-right: auto; }
.drawer-body { flex: 1; overflow: auto; padding: 18px; overscroll-behavior: contain; }
.detail-body { white-space: pre-wrap; line-height: 1.85; font-size: 14px; }
.drawer-body > button { margin-top: 14px; }
.drawer-body details { margin-top: 18px; }
.confirm { width: min(420px, calc(100vw - 32px)); border-radius: 16px; padding: 22px; }
.confirm p { margin: 8px 0 20px; color: var(--muted); white-space: pre-wrap; }
.confirm .row { justify-content: flex-end; }
.sheet { width: 100vw; max-width: 100vw; margin: auto 0 0; border-width: 1px 0 0; border-radius: 18px 18px 0 0; padding: 10px 12px calc(14px + env(safe-area-inset-bottom)); }
.sheet[open] { display: grid; gap: 3px; animation: enter .18s ease-out; }
.sheet::before { content: ""; width: 38px; height: 4px; margin: 0 auto 8px; border-radius: 2px; background: var(--line-strong); }

/* ── 设置 ─────────────────────────────────────────── */
.settings-grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 16px; align-items: start; }
.settings-grid .wide { grid-column: 1 / -1; }
.settings-grid .panel > p { color: var(--muted); margin: 4px 0 14px; font-size: 13px; }
.settings-grid label { font-size: 13px; font-weight: 550; }
#budget { width: 190px; font-variant-numeric: tabular-nums; }

/* ── 子代理 ───────────────────────────────────────── */
.agent-layout { display: grid; grid-template-columns: 260px minmax(0, 1fr); height: calc(100dvh - 190px); min-height: 380px; background: var(--surface); border: 1px solid var(--line); border-radius: var(--radius); overflow: hidden; box-shadow: var(--shadow); }
.agent-list { display: flex; flex-direction: column; gap: 4px; padding: 8px; overflow: auto; border-right: 1px solid var(--line); background: var(--raised); }
#agentList { display: grid; gap: 4px; }
.agent-list button { display: block; width: 100%; padding: 10px 12px; border: 0; background: transparent; text-align: left; white-space: pre-wrap; font-size: 12px; font-weight: 400; color: var(--muted); line-height: 1.6; }
.agent-list button::first-line { font-size: 14px; font-weight: 600; color: var(--text); }
.agent-list button:hover { background: var(--surface); }
.agent-list button[aria-current=true] { background: var(--accent-soft); }
.agent-chat { display: flex; flex-direction: column; min-width: 0; min-height: 0; }
.agent-chat-head { padding: 12px 18px; border-bottom: 1px solid var(--line); }
.agent-chat-head .hint { white-space: pre-wrap; max-height: 5.2em; overflow: auto; }
.agent-scroll { flex: 1; overflow: auto; padding: 16px 18px; display: flex; flex-direction: column; gap: 14px; }
#agentMessages { display: flex; flex-direction: column; gap: 14px; }
.agent-message { display: flex; flex-direction: column; align-items: flex-start; max-width: min(86%, 720px); }
.agent-message[data-direction=outgoing] { align-self: flex-end; align-items: flex-end; }
.agent-message small { font-size: 11px; color: var(--faint); margin-bottom: 4px; }
.agent-message p { padding: 10px 13px; border-radius: 4px 14px 14px 14px; background: var(--raised); border: 1px solid var(--line); white-space: pre-wrap; }
.agent-message[data-direction=outgoing] p { border-radius: 14px 4px 14px 14px; background: var(--accent-soft); border-color: var(--accent-line); }
.agent-message button { margin-top: 6px; min-height: 28px; padding: 2px 10px; font-size: 12px; }
#agentBack { display: none; }

/* ── 实时：四个镜头 ───────────────────────────────── */
${appColors}
.app-tile { display: grid; place-items: center; position: relative; flex: none; color: #fff; background: linear-gradient(145deg, var(--app-2), var(--app)); border-radius: 28%; }
.app-tile svg { width: 56%; height: 56%; }
.app-dot { display: inline-block; width: 9px; height: 9px; flex: none; margin-right: 7px; border-radius: 3px; background: var(--app, var(--faint)); }
.lens-tabs { display: none; }
.live-grid {
  display: grid; gap: 14px; grid-template-columns: minmax(0, 1fr) clamp(320px, 28vw, 420px);
  grid-template-rows: 236px minmax(480px, calc(100dvh - 378px)) auto;
  grid-template-areas: "star thoughts" "device thoughts" "pulse pulse";
}
[data-lens=star] { grid-area: star; }
[data-lens=device] { grid-area: device; }
[data-lens=thoughts] { grid-area: thoughts; }
[data-lens=pulse] { grid-area: pulse; }
.star-lens, .workspace-device, .workspace-thoughts, .pulse-card, .pulse-stat { background: var(--surface); border: 1px solid var(--line); border-radius: 18px; box-shadow: var(--shadow); min-width: 0; }
.workspace-device, .workspace-thoughts { display: flex; flex-direction: column; min-height: 0; overflow: hidden; }

/* 星图 */
.star-lens { display: flex; overflow: hidden; background:
  radial-gradient(60% 120% at 62% 50%, color-mix(in srgb, var(--accent) 12%, transparent), transparent 70%),
  radial-gradient(40% 90% at 96% 0%, color-mix(in srgb, var(--violet) 12%, transparent), transparent 70%),
  radial-gradient(40% 90% at 30% 100%, color-mix(in srgb, var(--info) 10%, transparent), transparent 70%), var(--surface); }
.live-hero { flex: none; width: min(280px, 34%); display: flex; flex-direction: column; align-items: flex-start; gap: 6px; padding: 20px 6px 18px 22px; }
.eyebrow { font-size: 12px; font-weight: 650; letter-spacing: 2px; color: var(--accent); }
.live-hero h2 { font-size: 17px; line-height: 1.45; }
.live-hero p:not(.eyebrow) { font-size: 12px; color: var(--muted); display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; }
.live-hero.live-error h2 { color: var(--danger); }
.live-controls { display: flex; gap: 2px 12px; flex-wrap: wrap; font-size: 12px; color: var(--muted); }
.live-hero > button { margin-top: auto; }
.live-duration { font-variant-numeric: tabular-nums; color: var(--accent); font-weight: 600; }
.workspace-connection { display: inline-flex; align-items: center; gap: 6px; }
.workspace-connection::before { content: ""; width: 7px; height: 7px; border-radius: 50%; background: var(--chart-unknown); }
.workspace-connection[data-state=connected]::before { background: var(--accent); box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 22%, transparent); }
.constellation { flex: 1; min-width: 0; min-height: 0; display: grid; place-items: center; }
.stars { display: block; width: 100%; height: 100%; overflow: visible; }
.stars-tall { display: none; max-width: 460px; }
:root[data-theme=dark] .star[data-app=terminal] { color: #a9b8cc; }
.star-orbit { fill: none; stroke: var(--line-strong); stroke-dasharray: 2 7; opacity: .8; }
.link-base { fill: none; stroke: var(--line-strong); stroke-width: 1.2; opacity: .7; transition: stroke .4s, opacity .4s; }
.link-flow { fill: none; stroke: var(--accent); stroke-width: 3.5; stroke-linecap: round; stroke-dasharray: 9 91; opacity: 0; }
.star-link[data-active=true] .link-base { stroke: var(--accent); stroke-width: 1.8; opacity: .6; }
.star-link[data-flow=out] .link-flow { opacity: 1; animation: flow-out 1.3s linear infinite; }
.star-link[data-flow=in] .link-flow { opacity: 1; animation: flow-in 1.3s linear infinite; }
@keyframes flow-out { from { stroke-dashoffset: 100; } to { stroke-dashoffset: 0; } }
@keyframes flow-in { from { stroke-dashoffset: 0; } to { stroke-dashoffset: 100; } }
.orb-stop-a { stop-color: #c9fbe6; } .orb-stop-b { stop-color: #27c79a; } .orb-stop-c { stop-color: #2f7df6; }
.orb :is(.orb-core, .orb-ring, .orb-spark, .orb-sat) { transform-box: fill-box; transform-origin: center; }
.orb-core { filter: drop-shadow(0 8px 16px color-mix(in srgb, var(--accent) 45%, transparent)); animation: breathe 4s ease-in-out infinite; }
.orb-shine { fill: #fff; opacity: .5; pointer-events: none; }
.orb-ring { fill: none; stroke: var(--accent); stroke-width: 1.5; opacity: 0; animation: ripple 4.5s ease-out infinite; }
.ring-2 { animation-delay: 1.5s; } .ring-3 { animation-delay: 3s; }
.orb-spark { fill: none; stroke: var(--violet); stroke-width: 4; opacity: 0; }
.orb-sat { opacity: 0; transition: opacity .4s; }
.orb-sat-path { fill: none; stroke: none; }
.orb-sat-dot { fill: var(--info); }
.orb-name { fill: #fff; font: 650 15px Georgia, serif; text-anchor: middle; pointer-events: none; }
.orb-label { fill: var(--muted); font-size: 12px; font-weight: 600; text-anchor: middle; }
@keyframes breathe { 50% { transform: scale(1.07); } }
@keyframes ripple { 0% { transform: scale(1); opacity: .55; } 100% { transform: scale(2.3); opacity: 0; } }
@keyframes spark-a { 0% { transform: scale(1); opacity: .9; } 100% { transform: scale(3); opacity: 0; } }
@keyframes spark-b { 0% { transform: scale(1); opacity: .9; } 100% { transform: scale(3); opacity: 0; } }
.constellation[data-spark=a] .orb-spark { animation: spark-a 1s ease-out; }
.constellation[data-spark=b] .orb-spark { animation: spark-b 1s ease-out; }
.constellation[data-phase=model] .orb-ring { animation-duration: 1.8s; }
.constellation[data-phase=model] .ring-2 { animation-delay: .6s; } .constellation[data-phase=model] .ring-3 { animation-delay: 1.2s; }
.constellation[data-phase=model] .orb-core { animation-duration: 1.3s; }
.constellation[data-phase=tool] .orb-sat { opacity: 1; animation: spin 2.4s linear infinite; }
.constellation[data-phase=rest] .orb-core { animation-duration: 8s; opacity: .7; }
.constellation[data-phase=rest] .orb-ring { animation-duration: 9s; }
.constellation[data-phase=paused] .orb * { animation-play-state: paused; }
.constellation[data-phase=paused] .orb-core { filter: grayscale(.9); opacity: .7; }
.constellation[data-phase=attention] .orb-stop-b { stop-color: #f5a524; } .constellation[data-phase=attention] .orb-stop-c { stop-color: #e5484d; }
.constellation[data-phase=attention] .orb-ring { stroke: var(--danger); }
.star { cursor: pointer; outline: none; color: var(--app); }
.star-disc { fill: var(--surface); stroke: var(--line-strong); stroke-width: 1.5; transition: fill .35s, stroke .35s, stroke-width .2s; }
.star-icon { fill: none; stroke: currentColor; stroke-width: 1.9; stroke-linecap: round; stroke-linejoin: round; pointer-events: none; transition: stroke .35s; }
.star-halo { fill: var(--app); opacity: 0; transform-box: fill-box; transform-origin: center; }
.star-name { fill: var(--muted); font-size: 12px; text-anchor: middle; }
.star-name.side-left { text-anchor: end; } .star-name.side-right { text-anchor: start; }
.star-count circle { fill: var(--text); } .star-count text { fill: var(--bg); font-size: 10px; font-weight: 650; text-anchor: middle; }
.star-count[data-empty=true] { display: none; }
.star[data-recent=true] .star-disc { stroke: var(--app); }
.star[data-active=true] .star-disc { fill: var(--app); stroke: var(--app); }
.star[data-active=true] .star-icon { stroke: #fff; }
.star[data-active=true] .star-halo { animation: halo 2s ease-out infinite; }
.star[data-active=true] .star-name { fill: var(--text); font-weight: 650; }
.star[data-viewing=true] .star-disc, .star:is(:hover, :focus-visible) .star-disc { stroke: var(--app); stroke-width: 3; }
@keyframes halo { 0% { transform: scale(.8); opacity: .35; } 100% { transform: scale(1.5); opacity: 0; } }

/* 屏幕：无外框。状态栏 → 主屏（小组件 + 应用图标）或打开的应用。 */
.screen { position: relative; background:
  radial-gradient(90% 70% at 0% 0%, color-mix(in srgb, var(--accent) 16%, transparent), transparent 65%),
  radial-gradient(80% 70% at 100% 8%, color-mix(in srgb, var(--info) 15%, transparent), transparent 65%),
  radial-gradient(80% 60% at 55% 100%, color-mix(in srgb, var(--violet) 13%, transparent), transparent 65%), var(--surface);
  background-size: 150% 150%; animation: drift 20s ease-in-out infinite alternate; }
@keyframes drift { from { background-position: 0% 0%; } to { background-position: 100% 100%; } }
.screen-status { display: flex; align-items: center; gap: 10px; padding: 9px 14px; font-size: 12px; font-weight: 600; }
.screen-clock { flex: 1; font-variant-numeric: tabular-nums; font-size: 13px; }
.screen-status-end { flex: 1; display: flex; justify-content: flex-end; align-items: center; min-width: 0; }
.device-mode { color: var(--muted); font-weight: 500; white-space: nowrap; }
.island { display: inline-flex; align-items: center; gap: 8px; padding: 5px 14px; border-radius: 999px; background: var(--text); color: var(--bg); white-space: nowrap; transition: background .4s; }
.island b { font-weight: 600; }
.island-dots { display: inline-flex; align-items: center; gap: 3px; height: 12px; }
.island-dots i { width: 5px; height: 5px; border-radius: 50%; background: currentColor; opacity: .4; }
.island:is([data-phase=model], [data-phase=tool]) .island-dots i { opacity: 1; background: var(--accent); animation: bounce 1s ease-in-out infinite; }
:root[data-theme=light] .island:is([data-phase=model], [data-phase=tool]) .island-dots i { background: #5fe3b9; }
.island-dots i:nth-child(2) { animation-delay: .15s !important; } .island-dots i:nth-child(3) { animation-delay: .3s !important; }
.island[data-phase=tool] .island-dots i { border-radius: 2px; animation-name: wave; }
.island[data-phase=attention] { background: var(--danger); color: #fff; }
.island[data-phase=paused] { background: var(--warn); color: #fff; }
.island[data-phase=rest] { background: var(--muted); }
@keyframes bounce { 0%, 60%, 100% { transform: translateY(0); } 30% { transform: translateY(-4px); } }
@keyframes wave { 0%, 100% { transform: scaleY(.6); } 50% { transform: scaleY(2.2); } }
.screen-banner { position: absolute; top: 46px; left: 50%; z-index: 5; translate: -50% 0; width: min(440px, calc(100% - 24px)); display: grid; justify-items: start; gap: 1px; min-height: 0; padding: 11px 16px; border: 1px solid var(--line); border-radius: 18px; text-align: left; background: color-mix(in srgb, var(--surface) 88%, transparent); backdrop-filter: blur(14px); box-shadow: var(--shadow-pop); animation: banner .45s cubic-bezier(.2, .9, .3, 1.25); }
.screen-banner span { font-size: 12px; color: var(--muted); font-weight: 400; }
@keyframes banner { from { transform: translateY(-70px); opacity: 0; } }
.screen[data-view=home] .screen-app, .screen[data-view=app] .screen-home { display: none; }
.screen-home { flex: 1; min-height: 0; overflow: auto; display: flex; flex-direction: column; gap: 22px; padding: 10px 20px 16px; }
.screen[data-anim=a] .screen-home { animation: home-a .32s ease-out; }
.screen[data-anim=b] .screen-home { animation: home-b .32s ease-out; }
@keyframes home-a { from { transform: scale(1.06); opacity: 0; } }
@keyframes home-b { from { transform: scale(1.06); opacity: 0; } }
.home-widgets { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px; }
.widget { padding: 14px 16px; border-radius: 20px; border: 1px solid var(--line); background: color-mix(in srgb, var(--surface) 78%, transparent); backdrop-filter: blur(10px); min-width: 0; }
.widget small { font-size: 11px; font-weight: 650; letter-spacing: 1.5px; color: var(--faint); }
.widget-now strong { display: block; margin: 4px 0 2px; font-size: 17px; line-height: 1.4; }
.widget-now p { font-size: 12px; color: var(--muted); display: -webkit-box; -webkit-line-clamp: 3; -webkit-box-orient: vertical; overflow: hidden; }
.home-recent { list-style: none; margin: 6px 0 0; padding: 0; display: grid; gap: 7px; }
.home-recent li { display: flex; align-items: center; font-size: 12px; min-width: 0; animation: enter .3s ease-out backwards; }
.home-recent li span { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-family: var(--mono); }
.home-recent li small { flex: none; margin-left: 8px; font-weight: 400; letter-spacing: 0; }
.home-recent .home-empty { color: var(--muted); }
.home-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(78px, 1fr)); gap: 16px 6px; justify-items: center; margin-top: auto; }
.home-app { flex-direction: column; gap: 5px; min-height: 0; padding: 4px; border: 0; background: transparent; font-size: 12px; font-weight: 500; animation: pop .45s cubic-bezier(.2, .9, .3, 1.3) backwards; }
.home-app:hover { background: transparent; }
.home-app:nth-child(2) { animation-delay: .04s; } .home-app:nth-child(3) { animation-delay: .08s; } .home-app:nth-child(4) { animation-delay: .12s; }
.home-app:nth-child(5) { animation-delay: .16s; } .home-app:nth-child(6) { animation-delay: .2s; } .home-app:nth-child(7) { animation-delay: .24s; }
.home-app .app-tile { width: 60px; height: 60px; box-shadow: 0 8px 18px color-mix(in srgb, var(--app) 38%, transparent); transition: transform .2s cubic-bezier(.2, .9, .3, 1.4); }
.home-app:hover .app-tile { transform: translateY(-4px) scale(1.06); }
.home-app:active .app-tile { transform: scale(.9); }
.home-app .count { position: absolute; top: -6px; right: -6px; border: 2px solid var(--surface); line-height: 16px; }
.dock-indicator { width: 5px; height: 5px; border-radius: 50%; }
[data-eve-active=true] .dock-indicator { background: var(--accent); box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 25%, transparent); }
@keyframes pop { from { transform: scale(.5); opacity: 0; } }
.home-hint { text-align: center; font-size: 12px; color: var(--faint); }
.screen-app { flex: 1; min-height: 0; display: flex; flex-direction: column; margin: 0 8px; background: var(--surface); border: 1px solid var(--line); border-bottom: 0; border-radius: 18px 18px 0 0; overflow: hidden; box-shadow: 0 -4px 24px #16203010; transform-origin: var(--open-x, 50%) var(--open-y, 70%); }
.screen[data-anim=a] .screen-app { animation: open-a .36s cubic-bezier(.2, .85, .25, 1); }
.screen[data-anim=b] .screen-app { animation: open-b .36s cubic-bezier(.2, .85, .25, 1); }
@keyframes open-a { from { transform: scale(.2); opacity: 0; border-radius: 60px; } }
@keyframes open-b { from { transform: scale(.2); opacity: 0; border-radius: 60px; } }
.device-appbar { display: flex; align-items: center; gap: 10px; padding: 9px 14px; border-bottom: 1px solid var(--line); }
.app-back { min-height: 32px; padding: 3px 10px 3px 6px; border: 0; background: transparent; color: var(--accent); font-size: 14px; white-space: nowrap; }
.device-app-title { display: flex; align-items: center; gap: 10px; min-width: 0; }
.device-app-title h2 { font-size: 15px; line-height: 1.3; }
.device-app-title p { font-size: 12px; color: var(--muted); }
.device-app-icon { width: 32px; height: 32px; }
.device-app-icon svg { width: 58%; height: 58%; }
.device-app { flex: 1; min-height: 0; overflow: hidden; }
.app-switcher { display: flex; justify-content: center; gap: 6px; padding: 6px 10px 4px; border-top: 1px solid var(--line); background: var(--raised); overflow-x: auto; scrollbar-width: none; }
.switch-app { flex-direction: column; gap: 3px; min-height: 0; padding: 3px 5px; border: 0; background: transparent; opacity: .55; transition: opacity .2s, transform .2s; }
.switch-app:hover { background: transparent; opacity: 1; transform: translateY(-2px); }
.switch-app[aria-pressed=true] { opacity: 1; }
.switch-app .app-tile { width: 32px; height: 32px; }
.switch-app[aria-pressed=true] .app-tile { box-shadow: 0 0 0 2px var(--surface), 0 0 0 4px var(--app); }
.device-status { display: flex; justify-content: space-between; gap: 12px; padding: 7px 16px; font-size: 12px; color: var(--muted); background: color-mix(in srgb, var(--surface) 70%, transparent); border-top: 1px solid var(--line); }
.device-status span:first-child { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.device-status span:last-child { white-space: nowrap; }

/* 脉络 */
.pulse { display: grid; gap: 14px; }
.pulse-head { display: flex; align-items: flex-end; justify-content: space-between; margin-top: 10px; }
.pulse-head h2 { font-size: 18px; }
.pulse-stats { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 12px; }
.pulse-stat { padding: 14px 18px; }
.pulse-stat strong { display: block; font-size: 26px; font-weight: 650; letter-spacing: -.5px; line-height: 1.3; font-variant-numeric: tabular-nums; }
.pulse-stat span { font-size: 12px; color: var(--muted); }
.pulse-stat[data-tone=thought] strong { color: var(--accent); }
.pulse-stat[data-tone=tool] strong { color: var(--warn); }
.pulse-stat[data-tone=model] strong { color: var(--info); }
.pulse-stat[data-tone=error] strong { color: var(--danger); }
.pulse-grid { display: grid; grid-template-columns: minmax(0, 1.8fr) minmax(0, 1fr); gap: 14px; }
.pulse-card { padding: 18px 20px; }
.pulse-card h3 { font-size: 15px; }
.river { display: block; width: 100%; min-width: 620px; height: auto; margin-top: 8px; }
.river-lane { stroke: var(--line-strong); stroke-dasharray: 3 7; animation: lane 1.4s linear infinite; }
@keyframes lane { to { stroke-dashoffset: -10; } }
.river-lane-name { fill: var(--faint); font-size: 12px; }
.river-marks { animation: wipe 1.2s cubic-bezier(.3, .7, .2, 1); }
@keyframes wipe { from { clip-path: inset(0 100% 0 0); } to { clip-path: inset(0 0 0 0); } }
.river-mark { cursor: pointer; outline: none; }
.river-mark > :not(title) { transform-box: fill-box; transform-origin: center; transition: transform .15s; }
.river-mark:is(:hover, :focus) > :not(title) { stroke: var(--text); stroke-width: 1.5; }
.river-mark:is(:hover, :focus) > circle { transform: scale(1.25); }
.river-model rect { fill: color-mix(in srgb, var(--info) 38%, transparent); }
.river-thought circle { fill: var(--accent); }
.river-quiet circle { fill: var(--surface); stroke: var(--faint); stroke-width: 1.5; }
.river-tool rect { fill: var(--chart-unknown); }
.river-toolfail rect, .river-error path { fill: var(--danger); }
.river-notice rect { fill: var(--violet); }
.river-mark.pending > :not(title), .river-now { animation: blink 1.2s ease-in-out infinite; }
.river-mark.fresh > :not(title) { animation: pop .6s cubic-bezier(.2, .9, .3, 1.4); }
.river-now { stroke: var(--accent); stroke-width: 1.5; stroke-dasharray: 3 4; }
@keyframes blink { 50% { opacity: .35; } }
.pulse-readout { display: flex; align-items: center; justify-content: space-between; gap: 12px; min-height: 44px; margin-top: 8px; padding: 8px 13px; border-radius: 10px; background: var(--raised); font-size: 13px; color: var(--muted); }
.usage-seg { fill: none; stroke: var(--app); transition: stroke-dasharray .9s cubic-bezier(.3, .7, .2, 1), stroke-dashoffset .9s cubic-bezier(.3, .7, .2, 1); }
.pulse-usage .budget-viz { margin-top: 14px; }
button.legend-row { width: 100%; justify-content: space-between; color: var(--text); font-size: 13px; padding: 2px 0; }
button.legend-row span { display: inline-flex; align-items: center; }
.rhythm { display: block; width: 100%; height: 56px; margin-top: 10px; }
.rhythm rect { fill: var(--chart-rest); transition: height .6s cubic-bezier(.3, .7, .2, 1), y .6s cubic-bezier(.3, .7, .2, 1), fill .4s; }
.rhythm rect[data-level=low] { fill: color-mix(in srgb, var(--accent) 40%, var(--chart-rest)); }
.rhythm rect[data-level=mid] { fill: color-mix(in srgb, var(--accent) 72%, var(--chart-rest)); }
.rhythm rect[data-level=high] { fill: var(--accent); }

.workspace-empty { height: 100%; min-height: 180px; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 6px; padding: 28px; text-align: center; color: var(--muted); }
.workspace-empty > span { font-size: 28px; color: var(--faint); }
.workspace-empty h3 { color: var(--text); }
.workspace-empty p { max-width: 360px; font-size: 13px; }
.workspace-meta { font-size: 12px; color: var(--muted); }
.workspace-tag { display: inline-flex; flex: none; padding: 1px 8px; border: 1px solid var(--line-strong); border-radius: 999px; font-size: 11px; color: var(--muted); }
.workspace-inline-note { margin: 10px 0; font-size: 12px; color: var(--muted); }

/* 随记 */
.thoughts-heading { display: flex; align-items: center; justify-content: space-between; gap: 10px; padding: 12px 16px 8px; }
.thought-count { font-size: 12px; color: var(--muted); }
.live-filters { display: flex; gap: 6px; padding: 0 16px; flex-wrap: wrap; }
.live-filter { min-height: 30px; padding: 3px 12px; border-radius: 999px; font-size: 12px; color: var(--muted); }
.live-filter[aria-pressed=true] { background: var(--accent-soft); border-color: var(--accent-line); color: var(--accent); font-weight: 600; }
.stream-caption { padding: 8px 16px; font-size: 12px; color: var(--faint); border-bottom: 1px solid var(--line); }
.live-feed { flex: 1; min-height: 0; overflow: auto; padding: 14px 16px; overscroll-behavior: contain; overflow-anchor: none; scrollbar-width: thin; }
.stream-entry { margin-bottom: 22px; min-width: 0; }
.stream-entry[data-new=true] { animation: enter .3s ease-out; }
.stream-meta { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; margin-bottom: 5px; font-size: 12px; color: var(--faint); }
.stream-meta strong { color: var(--accent); font-size: 11px; letter-spacing: .8px; }
.stream-meta button { margin-left: auto; }
.stream-prose { white-space: pre-wrap; font-size: 15px; line-height: 1.85; }
.stream-decision .stream-prose { font-size: 13px; color: var(--muted); }
.stream-truncated { margin-top: 6px; font-size: 12px; color: var(--muted); }
.stream-operations { margin: 0 0 18px; padding-left: 12px; border-left: 2px solid var(--line); }
.stream-operations > summary { padding: 4px 0; font-size: 13px; }
.stream-operations > summary:hover { color: var(--accent); }
.stream-operation { padding: 9px 0; border-bottom: 1px solid var(--line); font-size: 12px; }
.stream-operation:last-child { border: 0; }
.stream-operation .stream-meta { margin: 0 0 2px; }
.stream-operation strong { font-weight: 550; }
.stream-operation p { white-space: pre-wrap; color: var(--muted); }
.stream-error { color: var(--danger); }
.stream-latest { align-self: center; margin: 8px auto; min-height: 32px; border-radius: 999px; font-size: 12px; color: var(--accent); }
.thoughts-foot { padding: 9px 16px; border-top: 1px solid var(--line); background: var(--raised); font-size: 12px; color: var(--faint); }
.workspace-thoughts .empty { border: 0; padding: 32px 12px; font-size: 13px; }

/* QQ 只读屏幕 */
.qq-screen { display: grid; grid-template-columns: 220px minmax(0, 1fr); height: 100%; }
.qq-sidebar { overflow: auto; border-right: 1px solid var(--line); background: var(--raised); scrollbar-width: thin; }
.qq-sidebar-title { padding: 10px 14px 6px; font-size: 12px; color: var(--faint); }
.qq-conversation { justify-content: flex-start; width: 100%; gap: 10px; padding: 9px 12px; border: 0; border-radius: 0; background: transparent; text-align: left; font-weight: 400; }
.qq-conversation:hover { background: var(--surface); }
.qq-conversation[aria-pressed=true] { background: var(--accent-soft); }
.qq-avatar { display: grid; place-items: center; width: 34px; height: 34px; flex: none; border-radius: 11px; background: var(--surface); border: 1px solid var(--line); color: var(--accent); font-size: 13px; font-weight: 600; }
.qq-conversation-info { flex: 1; min-width: 0; }
.qq-conversation-info strong { display: block; font-size: 13px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.qq-conversation-info p { font-size: 12px; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.qq-notifications { flex: none; min-width: 18px; padding: 0 5px; border-radius: 9px; background: var(--info); color: #fff; font-size: 11px; line-height: 18px; text-align: center; }
:root[data-theme=dark] .qq-notifications { color: #0d1a30; }
.qq-chat { display: flex; flex-direction: column; min-width: 0; min-height: 0; }
.qq-chat-header { display: flex; align-items: center; justify-content: space-between; gap: 10px; padding: 9px 16px; border-bottom: 1px solid var(--line); }
.qq-chat-header h3 { font-size: 14px; }
.qq-chat-header p { font-size: 12px; color: var(--muted); }
.qq-chat-messages { flex: 1; min-height: 0; overflow: auto; padding: 16px; display: flex; flex-direction: column; gap: 14px; scrollbar-width: thin; }
.qq-chat-messages > button { align-self: center; order: -1; }
.qq-message { display: flex; gap: 8px; align-items: flex-start; }
.qq-message[data-new=true] { animation: enter .24s ease-out; }
.qq-message .qq-avatar { width: 30px; height: 30px; border-radius: 10px; font-size: 12px; }
.qq-message-content { min-width: 0; max-width: min(82%, 640px); }
.qq-message-meta { display: flex; gap: 8px; flex-wrap: wrap; margin-bottom: 3px; font-size: 12px; color: var(--faint); }
.qq-bubble { padding: 9px 13px; border-radius: 4px 14px 14px 14px; background: var(--raised); border: 1px solid var(--line); white-space: pre-wrap; line-height: 1.7; }
.qq-message.outgoing { flex-direction: row-reverse; }
.qq-message.outgoing .qq-message-meta { justify-content: flex-end; }
.qq-message.outgoing .qq-bubble { border-radius: 14px 4px 14px 14px; background: var(--accent-soft); border-color: var(--accent-line); }
.qq-message-status { margin-top: 3px; text-align: right; font-size: 12px; color: var(--faint); }
.qq-message-status:is([data-state=failed], [data-state=unknown]) { color: var(--danger); font-weight: 550; }
.qq-attachment { display: grid; gap: 3px; margin-top: 6px; padding: 8px 11px; border: 1px solid var(--line); border-radius: 10px; font-size: 12px; color: var(--muted); }
.qq-attachment-status[data-state=ready] { color: var(--accent); }
.qq-attachment-status[data-state=failed] { color: var(--danger); }
.qq-attachment-path { font: 11px/1.6 var(--mono); }
.qq-chat-footer { padding: 7px 16px; border-top: 1px solid var(--line); font-size: 12px; color: var(--faint); }

/* 工具回执：左列表右详情 */
.workspace-app-split { display: grid; grid-template-columns: 220px minmax(0, 1fr); height: 100%; }
.workspace-oplist { overflow: auto; border-right: 1px solid var(--line); background: var(--raised); scrollbar-width: thin; }
.workspace-op { display: grid; justify-content: stretch; justify-items: start; gap: 1px; width: 100%; padding: 9px 13px; border: 0; border-radius: 0; background: transparent; text-align: left; font-weight: 400; }
.workspace-op:hover { background: var(--surface); }
.workspace-op[aria-pressed=true] { background: var(--accent-soft); }
.workspace-op strong { max-width: 100%; font: 550 12px/1.6 var(--mono); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.workspace-op span { font-size: 12px; color: var(--muted); }
.workspace-detail { overflow: auto; padding: 16px; min-width: 0; scrollbar-width: thin; }
.workspace-detail h3 { margin-bottom: 4px; }
.workspace-detail pre { max-height: none; }
.workspace-source { display: flex; align-items: center; gap: 4px 12px; flex-wrap: wrap; margin-bottom: 12px; font-size: 12px; color: var(--muted); }
.workspace-op-status:is([data-status=failed], [data-status=unknown]) { color: var(--danger); font-weight: 550; }
.workspace-raw { margin-top: 16px; }
.terminal-command { padding: 10px 13px; border-radius: 9px 9px 0 0; background: #101614; color: #9fe0c1; font: 12px/1.7 var(--mono); white-space: pre-wrap; }
.terminal-command::before { content: "$ "; color: #6d7b73; }
.terminal-output { padding: 4px 13px 10px; background: #101614; color: #dfe6e1; font: 12px/1.7 var(--mono); white-space: pre-wrap; }
.terminal-output:last-of-type { border-radius: 0 0 9px 9px; }
.terminal-stderr { color: #f0b39b; }
.terminal-status { margin-top: 10px; font-size: 12px; color: var(--muted); }
.file-path, .browser-address { margin: 8px 0; padding: 7px 11px; border-radius: 8px; background: var(--sunken); border: 1px solid var(--line); color: var(--muted); font: 12px/1.6 var(--mono); }
.file-row { display: flex; align-items: center; gap: 9px; padding: 8px 3px; border-bottom: 1px solid var(--line); font-size: 13px; }
.file-row span:first-child { color: var(--accent); }
.file-row small { margin-left: auto; }
.note-paper, .workspace-detail-card { padding: 14px 16px; margin-top: 10px; border: 1px solid var(--line); border-radius: 11px; background: var(--raised); }
.note-paper h3 { font-size: 16px; }
.note-body { margin-top: 8px; white-space: pre-wrap; line-height: 1.8; font-size: 13px; }
.workspace-detail-list { display: grid; }
.workspace-detail-card p { white-space: pre-wrap; color: var(--muted); font-size: 13px; }
.workspace-facts { display: grid; grid-template-columns: minmax(80px, auto) minmax(0, 1fr); gap: 5px 14px; margin: 10px 0; font-size: 12px; }
.workspace-facts dt { color: var(--muted); }
.workspace-facts dd { white-space: pre-wrap; }
.tools-screen { display: flex; flex-direction: column; height: 100%; min-height: 0; }
.tools-toolbar { display: flex; gap: 6px; padding: 8px 12px; border-bottom: 1px solid var(--line); }
.tools-tab { min-height: 30px; padding: 3px 12px; border-radius: 999px; font-size: 12px; color: var(--muted); }
.tools-tab[aria-pressed=true] { background: var(--accent-soft); border-color: var(--accent-line); color: var(--accent); font-weight: 600; }
.tools-content { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.tools-filter { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; padding: 8px 12px; border-bottom: 1px solid var(--line); }
.tools-filter select { min-height: 30px; padding: 3px 8px; font-size: 12px; }
.tools-operations { flex: 1; min-height: 0; }
.tools-catalog { overflow: auto; padding: 14px 16px; }
.tools-namespace { margin-bottom: 18px; }
.tools-namespace h3 { font-family: var(--mono); color: var(--accent); }
.tools-methods { display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 6px; margin-top: 8px; }
.tools-method { display: block; min-height: 0; padding: 8px 10px; text-align: left; font: 12px/1.5 var(--mono); }
.tools-method span { display: block; margin-top: 2px; color: var(--muted); font: 11px/1.5 system-ui, sans-serif; }
.tools-detail-label { margin-top: 14px; font-size: 12px; color: var(--muted); }
.ws-web { display: grid; gap: 12px; min-width: 0; }
.ws-web-chrome { display: flex; align-items: center; gap: 9px; font-size: 12px; color: var(--muted); }
.ws-web-chrome strong { color: var(--text); }
.ws-web-badge { margin-left: auto; padding: 1px 8px; border: 1px solid var(--warn-line); border-radius: 999px; color: var(--warn); font-size: 11px; }
.ws-web-query { padding: 10px 16px; border: 1px solid var(--line-strong); border-radius: 999px; white-space: pre-wrap; }
.ws-web-query small { display: block; font-size: 11px; }
.ws-web .browser-address, .ws-web .workspace-inline-note { margin: 0; }
.ws-web-context { display: flex; gap: 4px 12px; flex-wrap: wrap; font-size: 12px; color: var(--muted); }
.ws-web-results { display: grid; gap: 10px; }
.ws-web-result { padding: 12px 14px; border: 1px solid var(--line); border-radius: 11px; min-width: 0; }
.ws-web-result h4 { margin: 0 0 6px; color: var(--info); font-size: 14px; white-space: pre-wrap; }
.ws-web-result p { margin-top: 8px; font-size: 13px; white-space: pre-wrap; }
.ws-web-document { padding: 16px 18px; border: 1px solid var(--line); border-radius: 11px; min-width: 0; }
.ws-web-document h3 { font-size: 17px; margin-bottom: 10px; }
.ws-web-body { white-space: pre-wrap; line-height: 1.9; font-size: 13px; }
.ws-web-pagination { display: grid; gap: 6px; padding: 9px 12px; border: 1px solid var(--line); border-radius: 9px; background: var(--raised); font-size: 12px; color: var(--muted); }
.ws-web-pagination progress { width: 100%; height: 5px; accent-color: var(--accent); }
.ws-web-source { font-size: 12px; color: var(--faint); }

/* ── 窄桌面 / 平板 ────────────────────────────────── */
@media (max-width: 1180px) {
  :root { --nav-width: 76px; }
  .sidebar { padding: 16px 10px; align-items: center; }
  .sidebar .brand div, .sidebar .nav-button span:not(.count) { display: none; }
  .sidebar .nav-button { justify-content: center; padding: 8px; position: relative; }
  .sidebar .nav-button .count { position: absolute; top: 2px; right: 6px; margin: 0; }
  .side-bottom, .sidebar nav { width: 100%; }
  .tiles { grid-template-columns: repeat(2, minmax(0, 1fr)); }
  .grid-two { grid-template-columns: minmax(0, 1fr); }
  .qq-screen, .workspace-app-split { grid-template-columns: 170px minmax(0, 1fr); }
}

/* ── 窄屏：四个镜头改为分页切换，各自占满余下高度 ── */
@media (max-width: 980px) {
  #live:not([hidden]) { display: flex; flex-direction: column; height: calc(100dvh - 114px); }
  .lens-tabs { display: flex; flex: none; width: 100%; margin-bottom: 10px; }
  .lens-tabs button { flex: 1; }
  .live-grid { flex: 1; min-height: 0; display: block; }
  .live-grid > [data-lens] { height: 100%; min-height: 400px; }
  .live-grid[data-mobile-view=star] > :not([data-lens=star]),
  .live-grid[data-mobile-view=device] > :not([data-lens=device]),
  .live-grid[data-mobile-view=thoughts] > :not([data-lens=thoughts]),
  .live-grid[data-mobile-view=pulse] > :not([data-lens=pulse]) { display: none; }
  .star-lens { flex-direction: column; }
  .live-hero { width: 100%; padding: 16px 18px 0; }
  .live-hero > button { margin-top: 4px; }
  .stars-wide { display: none; }
  .stars-tall { display: block; }
  .pulse { overflow: auto; align-content: start; }
  .pulse-head { margin-top: 0; }
  .pulse-grid { grid-template-columns: minmax(0, 1fr); }
}

/* ── 手机 ─────────────────────────────────────────── */
@media (max-width: 760px) {
  body { font-size: 15px; }
  input, select { font-size: 16px; }
  .shell { display: block; }
  .sidebar { display: none; }
  .main { padding: 0 14px calc(var(--tabbar) + 28px + env(safe-area-inset-bottom)); }
  .topbar { margin: 0 -14px 14px; padding: 10px 14px; border-bottom-color: var(--line); }
  .topbar .mark { display: grid; width: 30px; height: 30px; font-size: 19px; border-radius: 9px; }
  .topbar-title p, .sync { display: none; }
  h1 { font-size: 18px; }
  .tabbar { position: fixed; inset: auto 0 0; z-index: 30; display: grid; grid-template-columns: repeat(5, 1fr); padding: 4px 6px calc(4px + env(safe-area-inset-bottom)); background: color-mix(in srgb, var(--surface) 92%, transparent); backdrop-filter: blur(14px); border-top: 1px solid var(--line); }
  .tabbar button { position: relative; flex-direction: column; gap: 1px; min-height: 50px; padding: 4px 2px; border: 0; background: transparent; color: var(--faint); font-size: 11px; font-weight: 500; border-radius: 10px; }
  .tabbar button[aria-current=page] { background: transparent; color: var(--accent); font-weight: 650; }
  .tabbar svg.icon { width: 23px; height: 23px; }
  .tabbar .count { position: absolute; top: 2px; left: calc(50% + 6px); margin: 0; }
  .toast { bottom: calc(var(--tabbar) + 16px + env(safe-area-inset-bottom)); }
  .panel { padding: 16px; }
  .hero { padding: 18px; }
  .hero h2 { font-size: 19px; }
  .hero .row, .hero .row button { width: 100%; }
  .tiles { gap: 10px; }
  .tile { padding: 13px 14px; }
  .tile .number { font-size: 21px; }
  .budget-viz { gap: 16px; }
  .ring { width: 108px; }
  .ring-label strong { font-size: 20px; }
  .settings-grid { grid-template-columns: minmax(0, 1fr); }
  .item-head { flex-wrap: wrap; }
  .searchbar input { flex-basis: 100%; }
  .searchbar select { flex: 1; }
  .drawer { width: 100vw; border: 0; }
  .view-head { align-items: stretch; }

  #live:not([hidden]) { height: calc(100dvh - 102px - var(--tabbar) - env(safe-area-inset-bottom)); }
  .pulse-stats { grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 10px; }
  .pulse-stat strong { font-size: 22px; }
  .pulse-card { padding: 14px; }
  .home-widgets { grid-template-columns: minmax(0, 1fr); }
  .screen-home { padding: 8px 14px 14px; gap: 16px; }
  .screen-app { margin: 0 4px; }
  .device-mode { display: none; }
  /* 会话与操作列表改为顶部横向滑动，正文占满宽度。 */
  .qq-screen, .workspace-app-split { grid-template-columns: minmax(0, 1fr); grid-template-rows: auto minmax(0, 1fr); }
  .qq-sidebar, .workspace-oplist { display: flex; gap: 6px; padding: 8px 10px; overflow-x: auto; overflow-y: hidden; border-right: 0; border-bottom: 1px solid var(--line); scrollbar-width: none; }
  .qq-sidebar-title, .qq-conversation .qq-avatar, .qq-conversation-info p { display: none; }
  .qq-conversation, .workspace-op { flex: none; width: auto; max-width: 180px; min-height: 34px; padding: 4px 12px; border: 1px solid var(--line); border-radius: 999px; background: var(--surface); }
  .qq-conversation[aria-pressed=true], .workspace-op[aria-pressed=true] { border-color: var(--accent-line); }
  .workspace-op { display: flex; gap: 8px; align-items: center; }
  .workspace-op span { white-space: nowrap; }
  .qq-message .qq-avatar { display: none; }
  .qq-message-content { max-width: 90%; }
  .qq-chat-messages { padding: 12px; }
  .workspace-detail { padding: 12px; }

  .agent-layout { display: block; height: calc(100dvh - var(--tabbar) - 190px - env(safe-area-inset-bottom)); }
  .agent-list { height: 100%; border: 0; }
  .agent-chat { height: 100%; }
  .agent-layout[data-pane=list] .agent-chat, .agent-layout[data-pane=chat] .agent-list { display: none; }
  #agentBack { display: inline-flex; margin-bottom: 6px; }
  .agent-message { max-width: 92%; }
}
@media (min-width: 761px) { .sheet { display: none; } }
@media (prefers-reduced-motion: reduce) { * { animation: none !important; transition: none !important; scroll-behavior: auto !important; } }

.workspace-value{border-left:2px solid var(--line);padding-left:12px;margin:10px 0}.workspace-value>.tools-detail-label{overflow-wrap:anywhere}
`;
