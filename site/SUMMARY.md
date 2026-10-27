# Strand

<!-- The sidebar, in order: `## Section`, then `- [Label](path from site/)`;
     `@reference/…` are the generated reference's pages and `@tools/cli`
     the page made from `strand --help`. Each
     section's pages are owned by one writer; see each page's source.
     The pages teach using Strand. What is there for the repository's
     readers (how the project tests or holds what a page says: CI jobs,
     test names, budgets) goes between two HTML comments, `repo-only`
     and `/repo-only`, each on a line of its own; the site leaves it out
     of the page, its contents and the search (markdown.rs, `shown`). -->

## Start
- [Install](start/install.md)
- [Running a shell](start/running.md)

## Guide
- [Overview](../docs/guide/README.md)
- [1. The hello bar](../docs/guide/level-1.md)
- [2. Tokens and components](../docs/guide/level-2.md)
- [3. State and popups](../docs/guide/level-3.md)
- [4. Surfaces that come and go](../docs/guide/level-4.md)
- [5. Your own theme and services](../docs/guide/level-5.md)
- [Plugins](../docs/guide/plugins.md)

## How-to
- [A bar with a calendar popup](how-to/calendar-popup.md)
- [An app launcher](how-to/launcher.md)
- [A notification stack](how-to/notifications.md)
- [A volume and brightness OSD](how-to/osd.md)
- [A theme from your wallpaper](how-to/wallpaper-theme.md)
- [A settings file](how-to/settings-file.md)
- [A D-Bus method or an action](how-to/dbus-and-actions.md)

## Concepts
- [The language in brief](concepts/language.md)
- [State and reactivity](concepts/state.md)
- [Tokens and theming](concepts/theming.md)
- [Layout](concepts/layout.md)
- [Motion](concepts/motion.md)
- [Surfaces and monitors](concepts/surfaces.md)
- [Services](concepts/services.md)
- [Live reload](concepts/live-reload.md)
- [Performance](concepts/performance.md)

## Tools
- [Editor setup](tools/editor.md)
- [The strand command](@tools/cli)
- [Inspector](tools/inspector.md)
- [Reporting a glitch](tools/reporting.md)
- [Playground](playground.md)

## Reference
- [Elements](@reference/elements)
- [Services](@reference/services)
- [Types](@reference/types)
- [Functions](@reference/functions)
- [Tokens](@reference/tokens)

## Help
- [Troubleshooting](help/troubleshooting.md)
