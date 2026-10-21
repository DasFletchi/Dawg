# Dawg interface

Dawg is a working server console. Its design should feel like an ordinary, carefully made desktop utility. The previous dashboard treatment is an anti-reference, not a visual system to preserve.

## Primary workflow

On connection, open the existing server's Console directly. Reconnecting retains the selected server when it still exists. A user without any servers sees a short setup prompt. The server browser and the multi-server table are secondary navigation.

The console receives most of the window. Place the server name, state and lifecycle actions above it, followed by one compact CPU/memory/uptime row and Console/Settings/Properties tabs. Avoid three metric cards competing with the console.

## Typography

Embed Adobe Source Sans 3 Regular and Semibold from assets/fonts/source-sans-3, with its OFL license. Use it consistently for labels, buttons and headings. Body/control text is 13–14px, secondary text at least 12px, and the server heading about 22px. Console output uses the system monospace at 13px; timestamps are explicitly UTC. No all-caps eyebrows or typographic marketing headers.

## Surfaces and color

Use flat graphite surfaces: main #24272b, browser #1c1f23, console #1f2226, raised controls #31363c. Main text #ebeff3 and secondary text #adb6c0 must remain legible. Steel blue #94b7d4 denotes focus and selection; a darker blue primary button supports readable text. Green, amber and red express actual state. Standard error output alone does not establish an error; Java writes routine diagnostics there too.

One border separates regions when needed. Control radii are 4px. Use spacing and aligned columns to group content. Avoid nested framed cards, pill statuses, gradients, shadows, neon accents, logo tiles and placeholder icon glyphs.

## Layout and interactions

A modest server browser stays subordinate to the workspace. The optional server overview is a table with aligned fields and consistent row heights, not a collection of cards. Use a 4px spacing base with 8/12/16/24px group spacing. Default window is 1240×820; all workflows remain usable at 1050×700.

Create and settings forms have one readable column. Keep labels attached to fields, group Basic and Runtime settings, explain why Create is unavailable, and leave running-server settings genuinely noneditable. Do not hide errors behind a disabled button. Tabs retain their position and use a simple selected state. Do not display a redundant “Local manager connected” label. A real connection error earns an actionable banner. Public join addresses come from daemon observations or explicit configuration; never substitute localhost.

## Validation

Review the rendered empty state, an existing stopped server, a running console, creation, settings, properties, disconnected recovery, long server names and minimum window size. Exercise the primary path with the actual daemon and Java fixture. Check text contrast and input focus. A successful compilation or a web-markup detector cannot validate a native Iced layout.

## Sources

The project includes Anthropic frontend-design and Impeccable. This pass applies their product-interface, quieter and distill guidance: remove ornamental structure, retain functions, use familiar controls and inspect the rendered result. The skill copies and provenance are in .agents/skills.

## Browser administration

The browser follows the same graphite surfaces, embedded typography, console-first routing and secondary server navigation. Add Files and Network tabs using familiar table rows, breadcrumbs and form controls. File operations show actual results and progress. State why writes are unavailable while a game runs. Keep account setup in authenticated administration and never present an unassigned tunnel address as ready. Support narrow browser windows without hiding lifecycle controls or introducing marketing components.
