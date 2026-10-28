import json

# Opus 5.5 official prices $/1M:
inp, out, cr, cc5m, cc1h = 4, 20, 0.20, 5, 8

expr = ("tier(\"base\", p * {inp} + c * {out} + cr * {cr} + cc * {cc5m} + cc1h * {cc1h})"
        .format(inp=inp, out=out, cr=cr, cc5m=cc5m, cc1h=cc1h))
print(expr)

p, c, r, w5, w1 = 1000, 500, 10000, 2000, 3000
cost = p*inp + c*out + r*cr + w5*cc5m + w1*cc1h
print("USD:", cost / 1_000_000)
print("quota before group:", cost / 1_000_000 * 500000)
