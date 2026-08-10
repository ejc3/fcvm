#!/usr/bin/env python3
"""charts.py — render the corrected run's headline figures as standalone SVGs.

Reads `corrected.json` (written by analyze.py) and emits `charts/*.svg`. Stdlib
only, so it runs anywhere the rest of the harness does.

Every figure carries its uncertainty: bars that represent a bootstrap median get
a 95% CI whisker, and the density figure plots the fitted line WITH its intercept
rather than a slope alone. A figure that cannot be drawn from the data is skipped
with a message instead of being drawn from defaults.

Palette: categorical slots are assigned in fixed order and never cycled; both
light and dark steps are declared so the SVG is legible on either surface.
"""
import json
import math
import os
import sys

# categorical slots, fixed order (light, dark)
SERIES = [
    ("#2a78d6", "#3987e5"),   # 1 blue
    ("#eb6834", "#d95926"),   # 2 orange
    ("#1baf7a", "#199e70"),   # 3 aqua
    ("#eda100", "#c98500"),   # 4 yellow
    ("#e87ba4", "#d55181"),   # 5 magenta
    ("#008300", "#008300"),   # 6 green
    ("#4a3aa7", "#9085e9"),   # 7 violet
]
SURFACE = ("#fcfcfb", "#1a1a19")
INK = ("#0b0b0b", "#ffffff")
INK2 = ("#52514e", "#c3c2b7")
GRID = ("#e3e2df", "#333331")


def esc(s):
    return (str(s).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;"))


def head(w, h, title, subtitle=""):
    """SVG preamble: roles as CSS custom properties, dark steps under both the
    OS media query and an explicit data-theme scope so a theme toggle wins."""
    css = f"""
  .s{{fill:var(--surface)}} .ink{{fill:var(--ink)}} .ink2{{fill:var(--ink2)}}
  .grid{{stroke:var(--grid);stroke-width:1}}
  .ax{{stroke:var(--ink2);stroke-width:1}}
  text{{font-family:ui-sans-serif,-apple-system,"Segoe UI",Roboto,sans-serif}}
  .t{{font-size:15px;font-weight:600}} .st{{font-size:11.5px}}
  .lbl{{font-size:11px}} .val{{font-size:11px;font-weight:600}}
  .whisk{{stroke:var(--ink);stroke-width:1.5;opacity:.75}}
  svg{{--surface:{SURFACE[0]};--ink:{INK[0]};--ink2:{INK2[0]};--grid:{GRID[0]}}}
"""
    for i, (lt, dk) in enumerate(SERIES):
        css += f"  svg{{--c{i}:{lt}}}\n"
    dark = f"--surface:{SURFACE[1]};--ink:{INK[1]};--ink2:{INK2[1]};--grid:{GRID[1]};"
    dark += "".join(f"--c{i}:{dk};" for i, (_, dk) in enumerate(SERIES))
    # Neutral is NOT a categorical slot: it marks the one segment that happens
    # AFTER the artifact exists (teardown), so hues are never cycled to cover it.
    css += "  svg{--neutral:#6e6d67;--neutral-ink:#ffffff}\n"
    dark += "--neutral:#a4a39b;--neutral-ink:#0b0b0b;"
    css += f"""
  @media (prefers-color-scheme:dark){{svg:where(:not([data-theme="light"])){{{dark}}}}}
  svg[data-theme="dark"]{{{dark}}}
"""
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" '
           f'viewBox="0 0 {w} {h}" role="img" aria-label="{esc(title)}">',
           f"<style>{css}</style>",
           f'<rect class="s" width="{w}" height="{h}"/>',
           f'<text class="t ink" x="16" y="26">{esc(title)}</text>']
    if subtitle:
        out.append(f'<text class="st ink2" x="16" y="44">{esc(subtitle)}</text>')
    return out


def nice_max(v):
    if v <= 0:
        return 1.0
    e = 10 ** math.floor(math.log10(v))
    for m in (1, 1.5, 2, 2.5, 3, 4, 5, 7.5, 10):
        if v <= m * e:
            return m * e
    return 10 * e


def barchart(path, title, subtitle, rows, unit="ms", xlab=""):
    """rows: [(label, value, lo, hi, series_idx)] — horizontal bars + 95% CI."""
    rows = [r for r in rows if r[1] is not None]
    if not rows:
        print(f"skip {path}: no data")
        return
    padl, padr, top, rowh = 190, 76, 62, 26
    w, h = 900, top + rowh * len(rows) + 44
    plot = w - padl - padr
    vmax = nice_max(max((r[3] if r[3] is not None else r[1]) for r in rows) * 1.02)
    o = head(w, h, title, subtitle)
    for k in range(6):
        x = padl + plot * k / 5
        o.append(f'<line class="grid" x1="{x:.1f}" y1="{top - 10}" x2="{x:.1f}" y2="{top + rowh * len(rows) - 6}"/>')
        o.append(f'<text class="lbl ink2" x="{x:.1f}" y="{top + rowh * len(rows) + 12}" '
                 f'text-anchor="middle">{vmax * k / 5:.0f}</text>')
    if xlab:
        o.append(f'<text class="lbl ink2" x="{padl + plot / 2:.0f}" y="{h - 8}" '
                 f'text-anchor="middle">{esc(xlab)}</text>')
    for i, (lab, v, lo, hi, ci) in enumerate(rows):
        y = top + i * rowh
        bw = max(2.0, plot * v / vmax)
        o.append(f'<text class="lbl ink" x="{padl - 10}" y="{y + 12}" text-anchor="end">{esc(lab)}</text>')
        # 2px surface gap between adjacent fills; 4px rounded data-end
        o.append(f'<rect x="{padl}" y="{y + 1}" width="{bw:.1f}" height="{rowh - 9}" '
                 f'rx="4" fill="var(--c{ci})"/>')
        if lo is not None and hi is not None and hi > lo:
            x1, x2 = padl + plot * lo / vmax, padl + plot * hi / vmax
            ym = y + (rowh - 8) / 2
            o.append(f'<line class="whisk" x1="{x1:.1f}" y1="{ym:.1f}" x2="{x2:.1f}" y2="{ym:.1f}"/>')
            for xx in (x1, x2):
                o.append(f'<line class="whisk" x1="{xx:.1f}" y1="{ym - 4:.1f}" x2="{xx:.1f}" y2="{ym + 4:.1f}"/>')
            txt = f"{v:.0f} ({lo:.0f}–{hi:.0f}){unit}"
        else:
            txt = f"{v:.0f}{unit}"
        o.append(f'<text class="val ink" x="{min(padl + plot * (hi or v) / vmax + 8, w - 6):.1f}" '
                 f'y="{y + 12}">{esc(txt)}</text>')
    o.append("</svg>")
    open(path, "w").write("\n".join(o))
    print("wrote", path)


def stacked_stages(path, title, subtitle, stages, total_label):
    """stages: [(name, ms, series_idx)] drawn as one stacked bar with a legend."""
    stages = [s for s in stages if s[1] and s[1] > 0]
    if not stages:
        print(f"skip {path}: no data")
        return
    w, top, barh = 900, 74, 46
    tot = sum(s[1] for s in stages)
    padl, padr = 16, 16
    plot = w - padl - padr
    h = top + barh + 30 + 22 * ((len(stages) + 2) // 3) + 16
    o = head(w, h, title, subtitle)
    x = padl
    for name, ms, ci in stages:
        bw = plot * ms / tot
        fill = "var(--neutral)" if ci < 0 else f"var(--c{ci})"
        ink = "var(--neutral-ink)" if ci < 0 else "#fff"
        o.append(f'<rect x="{x:.1f}" y="{top}" width="{max(0.5, bw - 2):.1f}" height="{barh}" '
                 f'rx="4" fill="{fill}"/>')
        if bw > 46:
            o.append(f'<text class="val" x="{x + bw / 2 - 1:.1f}" y="{top + barh / 2 + 4:.0f}" '
                     f'text-anchor="middle" fill="{ink}">{ms:.0f}</text>')
        x += bw
    o.append(f'<text class="lbl ink2" x="{padl}" y="{top + barh + 18}">0 ms</text>')
    o.append(f'<text class="val ink" x="{w - padr}" y="{top + barh + 18}" text-anchor="end">'
             f'{esc(total_label)}</text>')
    ly = top + barh + 42
    for i, (name, ms, ci) in enumerate(stages):
        col, rowi = i % 3, i // 3
        lx = padl + col * (plot / 3)
        yy = ly + rowi * 22
        lfill = "var(--neutral)" if ci < 0 else f"var(--c{ci})"
        o.append(f'<rect x="{lx}" y="{yy - 9}" width="10" height="10" rx="2" fill="{lfill}"/>')
        o.append(f'<text class="lbl ink" x="{lx + 16}" y="{yy}">{esc(name)} — {ms:.0f} ms</text>')
    o.append("</svg>")
    open(path, "w").write("\n".join(o))
    print("wrote", path)


def density_chart(path, title, subtitle, series, nmax):
    """series: [(name, rows[(N,total_MiB)], slope, intercept, series_idx)].
    Plots the measured points AND the fitted line including its intercept — the
    intercept is the whole point (a slope alone inverted the ranking at real N)."""
    series = [s for s in series if s[1]]
    if not series:
        print(f"skip {path}: no data")
        return
    w, h = 900, 470
    padl, padr, top, bot = 74, 168, 70, 52
    pw, ph = w - padl - padr, h - top - bot
    ymax = nice_max(max(max(v for _, v in s[1]) for s in series) * 1.06)
    o = head(w, h, title, subtitle)
    for k in range(6):
        y = top + ph - ph * k / 5
        o.append(f'<line class="grid" x1="{padl}" y1="{y:.1f}" x2="{padl + pw}" y2="{y:.1f}"/>')
        o.append(f'<text class="lbl ink2" x="{padl - 8}" y="{y + 4:.1f}" text-anchor="end">'
                 f'{ymax * k / 5:.0f}</text>')
    for n in range(0, nmax + 1, max(1, nmax // 8)):
        x = padl + pw * n / nmax
        o.append(f'<text class="lbl ink2" x="{x:.1f}" y="{top + ph + 18}" text-anchor="middle">{n}</text>')
    o.append(f'<text class="lbl ink2" x="{padl + pw / 2:.0f}" y="{h - 14}" text-anchor="middle">'
             f'concurrent shared-nothing requests (N)</text>')
    o.append(f'<text class="lbl ink2" x="16" y="{top - 12}">MiB</text>')

    def px(n):
        return padl + pw * n / nmax

    def py(v):
        return top + ph - ph * min(v, ymax) / ymax
    for name, rows, slope, icept, ci in series:
        if slope is not None and icept is not None:
            o.append(f'<line x1="{px(0):.1f}" y1="{py(icept):.1f}" x2="{px(nmax):.1f}" '
                     f'y2="{py(icept + slope * nmax):.1f}" stroke="var(--c{ci})" stroke-width="2" '
                     f'opacity=".55" stroke-dasharray="5 4"/>')
        for n, v in sorted(rows):
            o.append(f'<circle cx="{px(n):.1f}" cy="{py(v):.1f}" r="4.5" fill="var(--c{ci})" '
                     f'stroke="var(--surface)" stroke-width="2"/>')
    ly = top + 4
    for name, rows, slope, icept, ci in series:
        o.append(f'<rect x="{padl + pw + 14}" y="{ly - 9}" width="10" height="10" rx="2" fill="var(--c{ci})"/>')
        o.append(f'<text class="lbl ink" x="{padl + pw + 30}" y="{ly}">{esc(name)}</text>')
        ly += 16
        if slope is not None:
            o.append(f'<text class="lbl ink2" x="{padl + pw + 30}" y="{ly}">'
                     f'{slope:.0f} MiB/req + {icept:.0f}</text>')
            ly += 18
    o.append("</svg>")
    open(path, "w").write("\n".join(o))
    print("wrote", path)


def main():
    res = sys.argv[1]
    d = json.load(open(os.path.join(res, "corrected.json")))
    out = os.path.join(res, "charts")
    os.makedirs(out, exist_ok=True)

    # 1 — end-to-end stage decomposition (the headline number)
    pc = d.get("primary_cell") or {}
    st = pc.get("stages") or {}

    def med(k):
        return (st.get(k) or {}).get("median")
    # Seven categorical slots, assigned in fixed order and never cycled, plus a
    # neutral for the one post-artifact segment. Screenshot and DOM dump are one
    # segment ("produce the artifact") so the bar still sums without an 8th hue.
    order = [(["restore_ms"], "restore (clone)", 0),
             (["exec_handshake_ms"], "fcvm exec handshake", 6),
             (["exec_spawn_ms"], "guest command start", 1),
             (["net_ready_host_ms"], "first egress", 3),
             (["r_connect_ms"], "CDP handshake", 5),
             (["r_navigate_ms", "r_idle_ms"], "page load", 2),
             (["r_screenshot_ms", "r_dom_ms"], "screenshot + DOM (JPEG q80)", 4),
             # -1 = neutral: teardown happens AFTER the reply could have been sent
             (["destroy_ms"], "teardown (post-artifact)", -1)]
    stages = []
    for keys, lab, ci in order:
        vals = [med(k) for k in keys if med(k) is not None]
        stages.append((lab, sum(vals) if vals else None, ci))
    tot = med("total_ms")
    art = med("artifact_ms")
    n = (st.get("total_ms") or {}).get("n")
    if any(s[1] for s in stages):
        stacked_stages(
            os.path.join(out, "stage-decomposition.svg"),
            "Shared-nothing Chromium render — where a request's wall time goes",
            f"{pc.get('mode')} / {pc.get('arm')} / {pc.get('page')} page, n={n} requests; "
            f"stage medians (they sum to slightly less than the total: each is its own median)",
            stages,
            f"artifact {art:.0f} ms · total {tot:.0f} ms" if tot and art else "")

    # 2 — egress modes, interleaved, with CIs
    eg = d.get("egress_modes") or {}
    rows = []
    for i, (m, v) in enumerate(sorted(eg.items(), key=lambda kv: (kv[1].get("artifact_median_ms") or 0))):
        rows.append((m, v.get("artifact_median_ms"), (v.get("artifact_ci") or [None, None])[0],
                     (v.get("artifact_ci") or [None, None])[1], i % len(SERIES)))
    barchart(os.path.join(out, "egress-modes.svg"),
             "Egress mode — artifact latency (interleaved schedule, seeded)",
             "median of all fixture pages; whisker = 95% bootstrap CI; routed included (DAD stall fixed)",
             rows, xlab="ms from request start to screenshot in hand")

    # 3 — memory density, slope AND intercept, matched basis
    dens = d.get("density") or {}
    ser, nmax = [], 1
    for i, (cell, e) in enumerate(sorted(dens.items())):
        if "fits" not in e:
            continue
        f = (e["fits"] or {}).get("cgroup")
        if not f:
            continue
        pts = [(r[0], r[1]) for r in e.get("rows", [])]
        nmax = max(nmax, max((p[0] for p in pts), default=1))
        ser.append((cell, pts, f.get("slope_mib_per_clone"), f.get("intercept_mib"), i % len(SERIES)))
    pool = d.get("host_pool") or {}
    pf = (pool.get("fits") or {}).get("cgroup")
    if pf and pool.get("rows"):
        pts = [(r[0], r[1]) for r in pool["rows"]]
        nmax = max(nmax, max(p[0] for p in pts))
        ser.append(("host container pool", pts, pf.get("slope_mib_per_container"),
                    pf.get("intercept_mib"), 6))
    density_chart(os.path.join(out, "memory-density.svg"),
                  "Memory per concurrent request — cgroup basis (all processes of each clone)",
                  "points = measured cells; dashed = OLS fit, drawn WITH its intercept",
                  ser, nmax)

    # 4 — burst throughput, burst as the experimental unit
    b = d.get("bursts") or {}
    rows = []
    for i, (k, v) in enumerate(sorted(b.items())):
        rows.append((k, v.get("rps_median"), (v.get("rps_ci") or [None, None])[0],
                     (v.get("rps_ci") or [None, None])[1], i % len(SERIES)))
    barchart(os.path.join(out, "burst-throughput.svg"),
             "Burst throughput — the BURST is the experimental unit",
             "median over repeated bursts per cell; whisker = 95% bootstrap CI",
             rows, unit=" req/s", xlab="requests per second within a burst")


if __name__ == "__main__":
    sys.exit(main())
