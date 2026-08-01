import re, math
from PIL import Image, ImageDraw

D = "M23.748 4.651c-.254-.124-.364.113-.512.233-.051.04-.094.09-.137.137-.372.397-.806.657-1.373.626-.829-.046-1.537.214-2.163.848-.133-.782-.575-1.248-1.247-1.548-.352-.155-.708-.311-.955-.65-.172-.24-.219-.509-.305-.774-.055-.16-.11-.323-.293-.35-.2-.031-.278.136-.356.276-.313.572-.434 1.202-.422 1.84.027 1.436.633 2.58 1.838 3.393.137.094.172.187.129.323-.082.28-.18.553-.266.833-.055.179-.137.218-.328.14a5.5 5.5 0 0 1-1.737-1.179c-.857-.828-1.631-1.743-2.597-2.46a12 12 0 0 0-.689-.47c-.985-.957.13-1.743.387-1.836.27-.098.094-.433-.778-.428-.872.003-1.67.295-2.687.685a3 3 0 0 1-.465.136 9.6 9.6 0 0 0-2.883-.101c-1.885.21-3.39 1.1-4.497 2.622C.082 8.776-.231 10.854.152 13.02c.403 2.284 1.568 4.175 3.36 5.653 1.857 1.533 3.997 2.284 6.438 2.14 1.482-.085 3.132-.284 4.994-1.86.47.234.962.328 1.78.398.629.058 1.235-.031 1.705-.129.735-.155.684-.836.418-.961-2.155-1.004-1.682-.595-2.112-.926 1.095-1.295 2.768-3.598 3.284-6.733.05-.346.115-.834.108-1.114-.004-.171.035-.238.23-.257a4.2 4.2 0 0 0 1.545-.475c1.397-.763 1.96-2.016 2.093-3.517.02-.23-.004-.467-.247-.588M11.58 18.168c-2.088-1.642-3.101-2.183-3.52-2.16-.39.024-.32.472-.234.763.09.288.207.487.371.74.114.167.192.416-.113.603-.673.416-1.842-.14-1.897-.168-1.361-.801-2.5-1.86-3.301-3.306-.775-1.393-1.225-2.888-1.299-4.482-.02-.385.094-.522.477-.592a4.7 4.7 0 0 1 1.53-.038c2.131.311 3.946 1.264 5.467 2.774.868.86 1.525 1.887 2.202 2.89.72 1.066 1.494 2.082 2.48 2.915.348.291.626.513.892.677-.802.09-2.14.109-3.055-.615zm1.001-6.44a.306.306 0 0 1 .415-.287.3.3 0 0 1 .113.074.3.3 0 0 1 .086.214c0 .17-.136.307-.308.307a.303.303 0 0 1-.306-.307m3.11 1.596c-.2.081-.4.151-.591.16a1.25 1.25 0 0 1-.798-.254c-.274-.23-.47-.358-.551-.758a1.7 1.7 0 0 1 .015-.588c.07-.327-.007-.537-.238-.727-.188-.156-.426-.199-.689-.199a.6.6 0 0 1-.254-.078.253.253 0 0 1-.114-.358 1 1 0 0 1 .192-.21c.356-.202.767-.136 1.146.016.352.144.618.408 1.001.782.392.451.462.576.685.915.176.264.336.536.446.848.066.194-.02.353-.25.45"

toks = re.findall(r'[MmcCaAz]|-?\d*\.?\d+(?:[eE][-+]?\d+)?', D)
i = 0
cur = (0.0, 0.0); start = (0.0, 0.0)
subpaths = []; cur_path = []; cmd = None

def cubic(p0, p1, p2, p3, n=24):
    pts = []
    for k in range(n + 1):
        t = k / n; mt = 1 - t
        x = mt**3*p0[0] + 3*mt*mt*t*p1[0] + 3*mt*t*t*p2[0] + t**3*p3[0]
        y = mt**3*p0[1] + 3*mt*mt*t*p1[1] + 3*mt*t*t*p2[1] + t**3*p3[1]
        pts.append((x, y))
    return pts

def arc(p0, rx, ry, phi, laf, sf, p1):
    phi = math.radians(phi)
    cosphi, sinphi = math.cos(phi), math.sin(phi)
    dx = (p0[0] - p1[0]) / 2; dy = (p0[1] - p1[1]) / 2
    x1p = cosphi * dx + sinphi * dy
    y1p = -sinphi * dx + cosphi * dy
    rx = abs(rx); ry = abs(ry)
    lam = (x1p*x1p)/(rx*rx) + (y1p*y1p)/(ry*ry)
    if lam > 1:
        s = math.sqrt(lam); rx *= s; ry *= s
    num = rx*rx*ry*ry - rx*rx*y1p*y1p - ry*ry*x1p*x1p
    den = rx*rx*y1p*y1p + ry*ry*x1p*x1p
    coef = math.sqrt(max(num/den, 0)) if den != 0 else 0
    if laf == sf: coef = -coef
    cxp = coef * (rx * y1p / ry)
    cyp = -coef * (ry * x1p / rx)
    cx = cosphi*cxp - sinphi*cyp + (p0[0]+p1[0])/2
    cy = sinphi*cxp + cosphi*cyp + (p0[1]+p1[1])/2
    def ang(u, v):
        d = math.atan2(v[1], v[0]) - math.atan2(u[1], u[0])
        if d > math.pi: d -= 2*math.pi
        if d < -math.pi: d += 2*math.pi
        return d
    th1 = ang((1,0), ((x1p-cxp)/rx, (y1p-cyp)/ry))
    dth = ang(((x1p-cxp)/rx, (y1p-cyp)/ry), ((-x1p-cxp)/rx, (-y1p-cyp)/ry))
    if sf == 0 and dth > 0: dth -= 2*math.pi
    if sf == 1 and dth < 0: dth += 2*math.pi
    n = max(2, int(abs(dth) / (math.pi/2)) + 1)
    seg = dth / n
    pts = []
    for k in range(n + 1):
        t = th1 + k * seg
        x = cx + rx*math.cos(t)*cosphi - ry*math.sin(t)*sinphi
        y = cy + rx*math.cos(t)*sinphi + ry*math.sin(t)*cosphi
        pts.append((x, y))
    return pts

while i < len(toks):
    t = toks[i]
    if t in 'MmcCaAz':
        cmd = t; i += 1
        continue
    if cmd == 'z':
        cur_path.append(start)
        subpaths.append(cur_path); cur_path = []
        cur = start; i += 1
        continue
    if cmd in 'Mm':
        x = float(t); y = float(toks[i+1]); i += 2
        if cmd == 'm':
            x += cur[0]; y += cur[1]
            cur = (x, y); start = (x, y)
        else:
            cur = (x, y); start = (x, y)
            if cur_path: subpaths.append(cur_path)
            cur_path = [cur]
        cmd = 'L' if cmd == 'm' else 'M'
        continue
    if cmd in 'Cc':
        p1 = (cur[0]+float(t), cur[1]+float(toks[i+1])); i += 2
        p2 = (cur[0]+float(toks[i]), cur[1]+float(toks[i+1])); i += 2
        p3 = (cur[0]+float(toks[i]), cur[1]+float(toks[i+1])); i += 2
        pts = cubic(cur, p1, p2, p3)
        cur_path.extend(pts[1:]); cur = p3
        continue
    if cmd in 'Aa':
        rx = float(t); ry = float(toks[i+1]); phi = float(toks[i+2]); i += 3
        laf = int(float(toks[i])); sf = int(float(toks[i+1])); i += 2
        x = cur[0]+float(toks[i]); y = cur[1]+float(toks[i+1]); i += 2
        pts = arc(cur, rx, ry, phi, laf, sf, (x, y))
        cur_path.extend(pts[1:]); cur = (x, y)
        continue
    raise SystemExit("unhandled: " + t + " cmd=" + cmd)
if cur_path: subpaths.append(cur_path)

scale = 20
img = Image.new('RGB', (24*scale, 24*scale), '#fdf6e3')
dr = ImageDraw.Draw(img)
for sp in subpaths:
    if len(sp) > 2:
        dr.polygon([(x*scale, y*scale) for x, y in sp], fill='#4d6bfe')
img.save('.petcheck/whale_render.png')
print("subpaths:", len(subpaths), [len(s) for s in subpaths])
