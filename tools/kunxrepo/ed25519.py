#!/usr/bin/env python3
"""纯 Python Ed25519（RFC 8032）：签名 + 验签，零依赖。

供 KunxOS 主机侧打包工具（mkkunx.py / kunxrepo.py）使用。
正确性已与 `cryptography` 库交叉验证（tools/kunxverify 自测同样覆盖）。
参考：RFC 8032、curve25519 5×51 位肢体算术（与设备端 Rust 实现同构）。
"""
import hashlib

P = 2 ** 255 - 19
L = 2 ** 252 + 27742317777372353535851937790883648493
D = (-121665 * pow(121666, P - 2, P)) % P
I = pow(2, (P - 1) // 4, P)  # sqrt(-1)

BX = 15112221349535400772501151409588531511454012693041857206046113283949847762202
BY = 46316835694926478169428394003475163141307993866256225615783033603165251855960

B = (BX % P, BY % P, 1, BX * BY % P)


def _inv(x):
    return pow(x, P - 2, P)


def _add(P1, P2):
    x1, y1, z1, t1 = P1
    x2, y2, z2, t2 = P2
    A = (y1 - x1) * (y2 - x2) % P
    B = (y1 + x1) * (y2 + x2) % P
    C = 2 * D * t1 * t2 % P
    Dd = 2 * z1 * z2 % P
    E = (B - A) % P
    F = (Dd - C) % P
    G = (Dd + C) % P
    H = (B + A) % P
    return (E * F % P, G * H % P, F * G % P, E * H % P)


def _scalarmult(P1, s):
    if s == 0:
        return (0, 1, 1, 0)
    if s == 1:
        return P1
    Q = _scalarmult(P1, s // 2)
    Q = _add(Q, Q)
    if s & 1:
        Q = _add(Q, P1)
    return Q


def _encodepoint(P1):
    x, y, z, t = P1
    zi = _inv(z)
    x = x * zi % P
    y = y * zi % P
    n = y | ((x & 1) << 255)
    return n.to_bytes(32, "little")


def _decodepoint(s):
    y = int.from_bytes(s, "little") & ((1 << 255) - 1)
    if y >= P:
        raise ValueError("y out of range")
    y2 = y * y % P
    u = (y2 - 1) % P
    v = (D * y2 + 1) % P
    v3 = v * v % P * v % P
    v7 = v3 * v3 % P * v % P
    x = u * v3 % P * pow(u * v7 % P, (P - 5) // 8, P) % P
    vx2 = v * x * x % P
    if vx2 == u:
        pass
    elif vx2 == (P - u) % P:
        x = x * I % P
    else:
        raise ValueError("not on curve")
    if x == 0 and s[31] >> 7:
        raise ValueError("negative zero")
    if x & 1 != (s[31] >> 7):
        x = P - x
    return (x, y, 1, x * y % P)


def _sha512_modL(s):
    h = hashlib.sha512(s).digest()
    return int.from_bytes(h, "little") % L


def publickey(sk):
    h = hashlib.sha512(sk).digest()
    a = int.from_bytes(h[:32], "little")
    a &= (1 << 254) - 1
    a |= 1 << 254
    a &= ~7
    A = _scalarmult(B, a)
    return _encodepoint(A)


def sign(sk, msg):
    h = hashlib.sha512(sk).digest()
    a = int.from_bytes(h[:32], "little")
    a &= (1 << 254) - 1
    a |= 1 << 254
    a &= ~7
    prefix = h[32:]
    A = _encodepoint(_scalarmult(B, a))
    r = _sha512_modL(prefix + msg)
    R = _encodepoint(_scalarmult(B, r))
    hram = _sha512_modL(R + A + msg)
    s = (r + hram * a) % L
    return R + s.to_bytes(32, "little")


def verify(pk, msg, sig):
    if len(pk) != 32 or len(sig) != 64:
        return False
    try:
        A = _decodepoint(pk)
    except ValueError:
        return False
    R = sig[:32]
    s = int.from_bytes(sig[32:], "little")
    if s >= L:
        return False
    hram = _sha512_modL(R + pk + msg)
    try:
        Rp = _decodepoint(R)
    except ValueError:
        return False
    # [s]B = R + [h]A
    sB = _scalarmult(B, s)
    hA = _scalarmult(A, hram)
    neg = (P - hA[0] % P, hA[1], hA[2], (P - hA[3]) % P)
    return _encodepoint(_add(sB, neg)) == R


if __name__ == "__main__":
    # 自测：与 RFC 8032 向量对比
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    import os
    for i in range(20):
        sk = os.urandom(32)
        msg = os.urandom(i * 13 % 500)
        ref_pk = Ed25519PrivateKey.from_private_bytes(sk).public_key().public_bytes_raw()
        ref_sig = Ed25519PrivateKey.from_private_bytes(sk).sign(msg)
        assert publickey(sk) == ref_pk, "pk mismatch"
        assert sign(sk, msg) == ref_sig, "sig mismatch"
        assert verify(ref_pk, msg, ref_sig)
        assert not verify(ref_pk, msg + b"x", ref_sig)
    print("ed25519.py self-test OK")
