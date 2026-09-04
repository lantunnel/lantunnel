# -*- coding: utf-8 -*-
"""Render the README topology diagram, aligned in every language.

The diagram is box art in a fenced code block, so every line has to occupy the
same number of terminal columns. Padding it by character count is wrong the
moment a label contains CJK: those characters are two columns wide, so the
Chinese and Japanese boxes came out one to three columns too wide and the
borders no longer met.

This computes padding from East Asian Width instead, treating W and F as two
columns and everything else — including the box-drawing characters and the
arrows, which are formally Ambiguous — as one, which is how GitHub renders
them. Labels live in topology-diagram.json.

    python3 scripts/render-topology-diagram.py \
        scripts/topology-diagram.json /tmp/out.json

Then paste each rendered block into the matching README. Editing a label by
hand is fine; re-run this afterwards rather than counting spaces.
"""
import unicodedata, json, sys

def dw(s):
    return sum(2 if unicodedata.east_asian_width(c) in ('W', 'F') else 1 for c in s)

class Canvas:
    def __init__(self, h, w):
        self.h, self.w = h, w
        self.g = [[' '] * w for _ in range(h)]
    def put(self, r, c, s):
        assert 0 <= c, f"column {c} is negative: {s!r}"
        assert c + dw(s) <= self.w, f"{s!r} runs past the canvas at column {c}"
        for ch in s:
            self.g[r][c] = ch
            if unicodedata.east_asian_width(ch) in ('W', 'F'):
                self.g[r][c + 1] = ''
                c += 2
            else:
                c += 1
    def render(self):
        return '\n'.join(''.join(row).rstrip() for row in self.g)

def build(d):
    peerA, peerB = d['peerA'], d['peerB']
    direct, gw_title, gw_lines = d['direct'], d['gw_title'], d['gw_lines']
    relay_l, relay_r, footer = d['relay'], d['relay'], d['footer']

    peer_inner = max(dw(x) for x in peerA + peerB) + 4
    peer_w = peer_inner + 2
    direct_w = max(dw(x) for x in direct)
    gw_inner = max([dw(gw_title)] + [dw(x) for x in gw_lines]) + 6
    gw_w = gw_inner + 2
    # Wide enough for the Direct label, and also wide enough that the Gateway
    # box below still leaves room for an arrow and a few dashes on each side.
    gap = max(direct_w + 10, gw_w + 8 - peer_w)


    # The left-hand relay label is right-aligned into the space before the
    # Gateway box, so the whole diagram has to start far enough right that the
    # longest of those labels still fits. Laid out once at zero to measure it.
    pad = 2
    probe = 1 + pad + peer_w + gap + peer_w // 2
    probe_gw_l = (1 + pad + peer_w // 2 + probe) // 2 - gw_w // 2
    OUT_L = max(0, max(dw(t) for t in d['relay']) + 3 - probe_gw_l)
    pa_l = OUT_L + 1 + pad
    pa_r = pa_l + peer_w - 1
    pb_l = pa_r + 1 + gap
    pb_r = pb_l + peer_w - 1
    OUT_R = pb_r + pad + 1

    stemA = pa_l + peer_w // 2
    stemB = pb_l + peer_w // 2
    gw_l = (stemA + stemB) // 2 - gw_w // 2
    gw_r = gw_l + gw_w - 1

    body = max(len(gw_lines), len(relay_l))
    rows = 10 + body + 2
    right_max = max([dw(x) for x in relay_r] + [0])
    width = max(OUT_R + 1, gw_r + 3 + right_max) + 1
    cv = Canvas(rows, width)

    def hline(r, c0, c1, ch='─'):
        cv.put(r, c0, ch * (c1 - c0 + 1))
    def centre(text, c0, c1):
        return c0 + (c1 - c0 + 1 - dw(text)) // 2

    # outer Tunnel bracket
    cv.put(0, OUT_L, '┌'); cv.put(0, OUT_R, '┐')
    hline(0, OUT_L + 1, OUT_R - 1)
    lbl = ' ' + d['tunnel'] + ' '
    cv.put(0, centre(lbl, OUT_L + 1, OUT_R - 1), lbl)
    for r in (1, 6):
        cv.put(r, OUT_L, '│'); cv.put(r, OUT_R, '│')

    # peer boxes
    for r in (2, 5):
        cv.put(r, OUT_L, '│'); cv.put(r, OUT_R, '│')
    for l, r_ in ((pa_l, pa_r), (pb_l, pb_r)):
        cv.put(2, l, '┌'); cv.put(2, r_, '┐'); hline(2, l + 1, r_ - 1)
        cv.put(5, l, '└'); cv.put(5, r_, '┘'); hline(5, l + 1, r_ - 1)
    cv.put(1, OUT_L, '│'); cv.put(1, OUT_R, '│')
    for i, (a, b) in enumerate(zip(peerA, peerB)):
        r = 3 + i
        cv.put(r, OUT_L, '│'); cv.put(r, OUT_R, '│')
        cv.put(r, pa_l, '│'); cv.put(r, pa_r, '│')
        cv.put(r, pb_l, '│'); cv.put(r, pb_r, '│')
        cv.put(r, centre(a, pa_l + 1, pa_r - 1), a)
        cv.put(r, centre(b, pb_l + 1, pb_r - 1), b)
    # stems out of the peer boxes
    cv.put(5, stemA, '┬'); cv.put(5, stemB, '┬')
    cv.put(6, stemA, '│'); cv.put(6, stemB, '│')

    # direct lane between the peer boxes
    g0, g1 = pa_r + 1, pb_l - 1
    cv.put(3, g0, '◀'); cv.put(3, g1, '▶')
    seg = ' ' + direct[0] + ' '
    s = centre(seg, g0 + 1, g1 - 1)
    hline(3, g0 + 1, s - 1); cv.put(3, s, seg)
    hline(3, s + dw(seg), g1 - 1)
    if len(direct) > 1:
        cv.put(4, centre(direct[1], g0, g1), direct[1])

    # close the Tunnel bracket, stems pass through
    cv.put(7, OUT_L, '└'); cv.put(7, OUT_R, '┘')
    hline(7, OUT_L + 1, OUT_R - 1)
    cv.put(7, stemA, '┼'); cv.put(7, stemB, '┼')
    cv.put(8, stemA, '│'); cv.put(8, stemB, '│')

    # gateway box
    cv.put(8, gw_l, '┌'); cv.put(8, gw_r, '┐'); hline(8, gw_l + 1, gw_r - 1)
    cv.put(9, stemA, '└'); hline(9, stemA + 1, gw_l - 2); cv.put(9, gw_l - 1, '▶')
    cv.put(9, stemB, '┘'); hline(9, gw_r + 2, stemB - 1); cv.put(9, gw_r + 1, '◀')
    cv.put(9, gw_l, '│'); cv.put(9, gw_r, '│')
    cv.put(9, centre(gw_title, gw_l + 1, gw_r - 1), gw_title)
    for i in range(body):
        r = 10 + i
        cv.put(r, gw_l, '│'); cv.put(r, gw_r, '│')
        if i < len(gw_lines):
            cv.put(r, gw_l + 3, gw_lines[i])
        if i < len(relay_l):
            t = relay_l[i]
            cv.put(r, gw_l - 3 - dw(t), t)          # right-aligned, clear of the border
            cv.put(r, gw_r + 3, relay_r[i])         # left-aligned, clear of the border
    br = 10 + body
    cv.put(br, gw_l, '└'); cv.put(br, gw_r, '┘'); hline(br, gw_l + 1, gw_r - 1)
    cv.put(br + 1, centre(footer, gw_l, gw_r), footer)
    return cv.render()

if __name__ == '__main__':
    specs = json.load(open(sys.argv[1], encoding='utf-8'))
    out = {k: build(v) for k, v in specs.items()}
    json.dump(out, open(sys.argv[2], 'w', encoding='utf-8'), ensure_ascii=False, indent=1)
    for k, v in out.items():
        print(f"----- {k} -----"); print(v)
