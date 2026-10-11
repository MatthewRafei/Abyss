"""Renders the Abyss icon from the octopus artwork.

    make-icon.py ART.svg OUT.svg              app icon: light fill, dark outline
    make-icon.py --symbolic ART.svg OUT.svg   symbolic icon: plain silhouette
                                              that GTK recolours from CSS
"""
import re
import sys

args = sys.argv[1:]
symbolic = args[0] == "--symbolic"
if symbolic:
    args = args[1:]
src, out = args
svg = open(src).read()
paths = re.findall(r'<path\b[^>]*\bd="([^"]+)"', svg)
assert paths, "no paths"
vb = [float(v) for v in re.search(r'viewBox="([^"]+)"', svg).group(1).split()]
w, h = vb[2], vb[3]
pad = 0 if symbolic else 80
side = max(w, h) + 2 * pad
x0 = vb[0] - (side - w) / 2
y0 = vb[1] - (side - h) / 2
body = "\n".join(f'    <path d="{d}"/>' for d in paths)
credit = """Octopus artwork from the Etsy shops VectorWhiz
       (https://www.etsy.com/shop/VectorWhiz) and CreativeSweetArt
       (https://www.etsy.com/shop/CreativeSweetArt), used under a commercial
       licence; not covered by the GPL."""
if symbolic:
    note = f"Abyss symbolic icon. {credit} One flat colour; GTK recolours it."
    group = '<g fill="#000000">'
else:
    note = (f"Abyss icon. {credit} Light fill with a dark\n"
            "       outline so it reads on dark and light launchers alike.")
    group = ('<g fill="#f2ecf5" stroke="#1a1020" stroke-width="44" '
             'stroke-linejoin="round" paint-order="stroke">')
open(out, "w").write(f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x0:.2f} {y0:.2f} {side:.2f} {side:.2f}">
  <!-- {note} -->
  {group}
{body}
  </g>
</svg>
''')
print(len(paths), "paths", f"{side:.0f}px square", "(symbolic)" if symbolic else "")
