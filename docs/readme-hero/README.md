# README hero source

`hero.html` is the source of [`../images/hero.png`](../images/hero.png): hand-written HTML, CSS and
inline SVG. The only raster it loads is the app's own icon. The fonts in `fonts/` are under the SIL
Open Font License, with each license beside its font.

To render it at 2560x1440:

```sh
pip install playwright==1.64.1
playwright install chromium-headless-shell
python3 docs/readme-hero/render.py
```

A Linux host with no system fonts needs a fontconfig file with at least one font in it, through
`FONTCONFIG_FILE`, or Chromium won't decode the page's fonts.

The app screenshots in `../images/` are separate: genuine Retina gallery captures from `--demo`
fixtures.
