"""Bundle the source files into ambient-occlusion.json (an rbb theme export)."""
import json, pathlib
here = pathlib.Path(__file__).parent
templates = {n: (here / n).read_text() for n in ["layout.html", "index.html", "pm/base.html", "pm/folder.html", "pm/read.html"]}
out = {
    "rbb_theme": 1,
    "name": "ambient occlusion",
    "properties": {"brand": "#1f45e0", "colormode": "light"},
    "stylesheet": (here / "ambient-occlusion.css").read_text(),
    "templates": templates,
}
(here / "ambient-occlusion.json").write_text(json.dumps(out, indent=1, ensure_ascii=False) + "\n")
print("wrote", len(out["stylesheet"]), "bytes of CSS and", len(templates), "templates")
