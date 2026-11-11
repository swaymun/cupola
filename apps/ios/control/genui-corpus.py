#!/usr/bin/env python3
"""Generate portable HTML coverage and native-frame screenshots.

Use capture-genui.py for rendered HTML assertions through XCTest; AXe cannot
read WebKit text. No model work.
"""

from pathlib import Path
import argparse, json

ROOT = Path(__file__).resolve().parents[3]
BASE = """<!doctype html><html><head><meta charset="utf-8"><style>
*{box-sizing:border-box}h1{font-size:22px;margin:0 0 12px}h2{font-size:16px}p{margin:8px 0}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(145px,1fr));gap:10px}
.card{padding:16px;border:1px solid var(--border);border-radius:var(--radius);background:var(--card)}
.muted{color:var(--muted-foreground)}.pill{display:inline-block;border-radius:999px;padding:3px 10px;background:var(--accent-surface);color:var(--accent-surface-foreground)}
table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:8px;border-bottom:1px solid var(--border)}
button,input,select{font:inherit;padding:8px;border:1px solid var(--border);border-radius:var(--radius);color:var(--foreground);background:var(--secondary)}
</style></head><body>"""
PAGES = {
    "grid": (
        "Regional dashboard",
        '<div class="grid">'
        + "".join(
            f'<section class="card"><b>{n}</b><p style="font-size:28px;color:var(--chart-{i + 1})">{v}</p><span class="pill">Healthy</span></section>'
            for i, (n, v) in enumerate(
                [("North", 42), ("South", 31), ("East", 28), ("West", 19)]
            )
        )
        + "</div>",
    ),
    "js-table": (
        "Inventory built by JavaScript",
        '<table><thead><tr><th>Item</th><th>Stock</th></tr></thead><tbody id="rows"></tbody></table><script>document.getElementById("rows").innerHTML=[["Apples",24],["Pears",18],["Plums",9]].map(r=>`<tr><td>${r[0]}</td><td>${r[1]}</td></tr>`).join("")</script>',
    ),
    "svg-chart": (
        "Weekly deliveries",
        '<svg role="img" aria-label="Deliveries: Monday 30, Tuesday 60, Wednesday 90" viewBox="0 0 320 180" style="width:100%;height:180px"><g fill="var(--chart-1)"><rect x="35" y="110" width="55" height="30"/><rect x="125" y="80" width="55" height="60"/><rect x="215" y="50" width="55" height="90"/></g><g fill="var(--foreground)" font-size="14"><text x="40" y="165">Mon</text><text x="130" y="165">Tue</text><text x="220" y="165">Wed</text></g></svg>',
    ),
    "cdn-chart": (
        "External chart library",
        '<p id="status">External chart unavailable offline. Data: Jan 12, Feb 19, Mar 15.</p><canvas id="chart" height="180"></canvas><script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.8/dist/chart.umd.min.js"></script><script>if(window.Chart){new Chart(document.getElementById("chart"),{type:"bar",data:{labels:["Jan","Feb","Mar"],datasets:[{label:"Orders",data:[12,19,15]}]}});document.getElementById("status").textContent="Chart loaded"}</script>',
    ),
    "badges": (
        "Release overview",
        '<section class="card"><span class="pill">Ready</span> <span class="pill" style="background:var(--warning-surface);color:var(--warning-foreground)">Pending</span><p>Three packages ready for review.</p><span class="pill" style="background:var(--destructive-surface);color:var(--destructive-foreground)">Failed</span></section>',
    ),
    "color-mix": (
        "Blended category colors",
        '<div class="grid">'
        + "".join(
            f'<div class="card" style="background:color-mix(in srgb,var(--chart-{i}) 22%,var(--background))">Category {i}</div>'
            for i in range(1, 7)
        )
        + "</div>",
    ),
    "variables": (
        "Theme token palette",
        "".join(
            f'<p class="card" style="background:var(--{n});color:var(--foreground)">{n}</p>'
            for n in [
                "background",
                "card",
                "secondary",
                "accent-surface",
                "warning-surface",
                "destructive-surface",
            ]
        )
        + '<p class="muted">Muted foreground</p><code>Monospace sample</code>',
    ),
    "details": (
        "Travel notes",
        "<details open><summary>Entry requirements</summary><p>Passport and arrival form.</p></details><details><summary>Packing list</summary><p>Water, shoes, rain jacket.</p></details>",
    ),
    "sticky": (
        "Team schedule",
        '<div style="height:310px;overflow:auto;border:1px solid var(--border)"><div style="position:sticky;top:0;padding:12px;background:var(--secondary)">Schedule header stays visible</div>'
        + "".join(f"<p>Session {i}: Research workshop</p>" for i in range(1, 21))
        + "</div>",
    ),
    "wide-table": (
        "Quarterly comparison",
        '<div style="overflow-x:auto"><table style="min-width:780px"><thead><tr>'
        + "".join(
            f"<th>{x}</th>"
            for x in ["Region", "Jan", "Feb", "Mar", "Apr", "May", "Jun"]
        )
        + "</tr></thead><tbody>"
        + "".join(
            "<tr>"
            + "".join(f"<td>{x}</td>" for x in [r, 12, 24, 18, 31, 25, 42])
            + "</tr>"
            for r in ["North", "South", "East"]
        )
        + '</tbody></table></div><p class="muted">Scroll the table horizontally.</p>',
    ),
    "images": (
        "Field observations",
        '<figure style="margin:0"><img alt="Mountain at sunset" style="width:100%;border-radius:var(--radius)" src="data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAzMjAgMTgwIj48cmVjdCB3aWR0aD0iMzIwIiBoZWlnaHQ9IjE4MCIgZmlsbD0iI2VkZDJhNCIvPjxjaXJjbGUgY3g9IjIzMCIgY3k9IjUwIiByPSIzMCIgZmlsbD0iI2U5OTI0NSIvPjxwYXRoIGQ9Ik0wIDE4MEwxMjAgMzBMMjQwIDE4MFoiIGZpbGw9IiMzNDY1NTMiLz48L3N2Zz4="> <figcaption>A self-contained landscape image.</figcaption></figure>',
    ),
    "forms": (
        "Workshop preferences",
        '<form onsubmit="event.preventDefault();document.getElementById(\'result\').textContent=\'Saved locally: \'+document.getElementById(\'name\').value"><label>Name <input id="name" value="Alex"></label><p><label>Track <select><option>Design</option><option>Engineering</option></select></label></p><button>Save locally</button><p id="result" role="status">Nothing saved yet.</p></form>',
    ),
    "dynamic-height": (
        "Progressive report",
        '<p>Report begins here.</p><div id="later"></div><script>setTimeout(()=>{document.getElementById("later").innerHTML="<p>Delayed section is visible.</p>"+"<p>Additional finding</p>".repeat(12)},700)</script>',
    ),
    "responsive": (
        "Responsive planning board",
        '<style>.board{display:grid;grid-template-columns:1fr;gap:12px}@media(min-width:600px){.board{grid-template-columns:repeat(3,1fr)}}</style><div class="board">'
        + "".join(
            f'<section class="card"><h2>{x}</h2><p>Read source material</p><p>Record observations</p></section>'
            for x in ["Today", "Tomorrow", "Next week"]
        )
        + "</div>",
    ),
    "js-svg": (
        "Script-generated SVG chart",
        '<svg id="plot" viewBox="0 0 320 180" role="img" aria-label="Monthly rainfall" style="width:100%;height:180px"></svg><script>document.getElementById("plot").innerHTML=[40,90,65].map((h,i)=>`<rect x="${25+i*100}" y="${160-h}" width="60" height="${h}" fill="var(--chart-${i+1})"/>`).join("")</script>',
    ),
}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--phase", choices=["before", "after"], default="before")
    a = p.parse_args()
    a.output.mkdir(parents=True, exist_ok=True)
    fixtures = ROOT / "apps/ios/SnapshotTests/Fixtures"
    manifest = []
    steps = []
    for slug, (title, body) in PAGES.items():
        name = "genui-" + slug
        html = BASE + "<h1>" + title + "</h1>" + body + "</body></html>"
        (a.output / (name + ".html")).write_text(html)
        (fixtures / (name + ".html")).write_text(html)
        manifest.append({"name": name, "title": title})
        steps += [
            f"launch --fixture read-preview --fixture send-preview --fixture html-render-preview --arg=-html-render-page --arg={name} --arg=-diagnostics-theme --arg=cupola-light --arg=-diagnostics-font --arg=system",
            "wait --id html-render-page --timeout 30",
            "reveal down --id html-render-expand --in conversation-scroll",
            "tap --id html-render-expand",
            "wait --id expanded-message --timeout 10",
            f"shot genui-{a.phase}-{slug}",
            "swipe down --from-sheet",
        ]
    (a.output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (a.output / (a.phase + ".wctl")).write_text("\n".join(steps) + "\n")
    print(f"{len(manifest)} pages; scenario: {a.output / (a.phase + '.wctl')}")


if __name__ == "__main__":
    main()
