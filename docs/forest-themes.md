# Forest themes

Clearing, Canopy, Wafer and Die Shot are four themes on the modern recipe
(see [Modern themes](modern-themes.md)). None is a default: pick one from
the Theme menu or name it in the configuration.

```toml
theme = "clearing"             # also "canopy", "wafer" or "die-shot"
```

| Theme | Native appearance | Controls | Title | Top edge |
| --- | --- | --- | --- | --- |
| Clearing | light | discs | centred, Inter Medium | a highlight inside the border |
| Canopy | dark | vias (rings) | centred, Inter SemiBold | the accent, fading down the corners |
| Wafer | light | pads, neutral until the pointer is on them | centred, IBM Plex Sans Medium | an interference line |
| Die Shot | dark | bond pads | after the controls, Source Code Pro Medium, with a trace to a via | none; the border is the accent |

The controls are at the left in the order close, minimise, zoom. Each theme
has one artwork. Worn in its other appearance a theme keeps its shapes and
type and takes its sibling's palette and artwork: Clearing with Canopy,
Wafer with Die Shot.

## What each back end draws

The themes are data in the shared theme layer: the title band, the controls
and their states, the top edge, the border and the menus are drawn there and
are the same on every back end. What surrounds a window is the back end's.

| | Wayland session | X11 session |
| --- | --- | --- |
| Title band, controls, top edge, hairline border | yes | yes |
| Rounded corners | yes, all four, anti-aliased, client content included | yes, by window shape: not anti-aliased |
| Drop shadow | yes, the recipe's shadow | no: the X11 session does not composite, as for Obsidian, Washi and Relay |
| Lighter shadow under a window without focus | yes | no shadow |
| A window fading in while it rises (`open_ms`, `open_rise`, `open_curve`) | not drawn: windows appear at once | not drawn |
| A theme change cross-fading (`switch_ms`) | not drawn: the change is immediate | not drawn |
| Artwork | yes | yes |

The motion values are carried in `chrome.window` so that a back end with the
means can use them. A back end without them ignores them; nothing else about
the theme depends on them.

## Type

Inter and Source Code Pro are not carried in this repository. Where the
system has them installed, the titles and menus are set in them. Where it
has not, IBM Plex Sans stands in for Inter and IBM Plex Mono for Source Code
Pro; both are already carried for the modern themes, in their regular
weight, so the titles are then lighter than Medium.

## Earlier ids

On the desktop these themes were first drawn for they were called
`sf-clearing`, `sf-canopy`, `sf-wafer` and `sf-dieshot`. A configuration, a
saved theme or a saved wallpaper that names one of those is read as the
theme beside it. The earlier ids are never written.

## Artwork

`scripts/gen-forest-wallpapers.py` draws the four pictures; they are
committed under `crates/chonk-shell/assets/wallpapers/` and embedded in the
shell like every other artwork (about 1 MB for the four). See
`assets/wallpapers/SOURCES.md`.
