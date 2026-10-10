# night

A self-contained black theme for rbb. Warm white Onest typography, graphite surfaces, fine borders, red activity indicators, and an orbital dot-pattern illustration built in CSS. No external fonts, scripts, or image services.

## Installation

In Admin CP → Themes & templates, paste `night.json` into **Import a theme**. Imports receive an `(imported)` suffix; edit the imported theme and rename it to `night`. Leave **Available to** empty to offer it to everyone. The existing default theme is not changed.

Visitors select **night** in the footer's Theme selector. The current local board already has it installed as theme 3.

## Template system

rbb resolves MiniJinja templates from the selected theme, then its parent chain, then the embedded defaults. Theme CSS loads after the base CSS. This theme inherits Default and overrides only:

- `layout.html`: moon wordmark and black browser theme color; existing navigation, permissions, forms, CSRF fields, and footer selectors are retained.
- `index.html`: custom welcome area with real board statistics and existing guest/member actions; categories and all other index sections remain intact.

The remaining pages inherit their original templates and receive the new design through CSS tokens and component styles. night deliberately stays dark even if the visitor previously selected a light color mode; switching themes still works normally.

## Source and updates

`night.css`, `layout.html`, and `index.html` are the source files. `night.json` is the portable import artifact containing all three. After changing a source file, update its corresponding field in `night.json` and update the installed theme through the Admin CP. Importing again creates a separate theme.

## Verification

Checked homepage at 390, 852, and 1280 pixel widths; mobile thread reading; desktop forum listing and privacy page; Default ↔ night switching; and category collapse/expand. No horizontal page overflow in the checked views. Reduced-motion settings disable decorative transitions. Fonts are served locally. Existing default themes and global templates were not edited.
