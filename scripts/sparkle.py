#!/usr/bin/env python3
"""Sparkle's side of a release (PRD: Auth, security and trust, Distribution).

Sparkle checks every update against an EdDSA (Ed25519) signature, made with
the private key only the release has (the SPARKLE_PRIVATE_KEY Actions
secret), and the public key in the app's Info.plist (SUPublicEDKey). This
derives that public key from the private one, so the two can't disagree;
signs the DMG; and writes the appcast Sparkle reads. Ed25519 is RFC 8032's,
in Python's standard library alone, so it runs on any Mac or Linux box; the
self-test holds it to the RFC's vectors.

  sparkle.py keygen                 a new private key, base64 (for tests;
                                    Sparkle's own generate_keys -x works too)
  sparkle.py public-key             SUPublicEDKey, from $SPARKLE_PRIVATE_KEY
  sparkle.py sign FILE              FILE's signature, base64, with that key
  sparkle.py verify FILE SIG PUB    exits 0 if SIG is FILE's under PUB
  sparkle.py appcast --version V --url URL --file FILE --signature SIG
                     [--minimum-system 26.0] [--release-page URL]
                                    the appcast, on stdout
  sparkle.py self-test              RFC 8032's vectors, and the appcast

The private key is read from the environment only, never from arguments
(which `ps` shows), and is never printed. Sparkle exports it as base64 of
a 32-byte seed; a 96-byte key (an older Sparkle's: the 64-byte expanded
private key, then the public key) is read too.
"""

import base64
import email.utils
import hashlib
import os
import sys
import xml.etree.ElementTree as ET
from xml.sax.saxutils import escape, quoteattr

# RFC 8032, section 6: Ed25519.
P = 2**255 - 19
Q = 2**252 + 27742317777372353535851937790883648493


def _inv(x):
    return pow(x, P - 2, P)


D = -121665 * _inv(121666) % P
SQRT_M1 = pow(2, (P - 1) // 4, P)


def _add(a, b):
    x = (a[1] - a[0]) * (b[1] - b[0]) % P
    y = (a[1] + a[0]) * (b[1] + b[0]) % P
    c = 2 * a[3] * b[3] * D % P
    d = 2 * a[2] * b[2] % P
    e, f, g, h = y - x, d - c, d + c, y + x
    return (e * f, g * h, f * g, e * h)


def _mul(s, point):
    result = (0, 1, 1, 0)
    while s > 0:
        if s & 1:
            result = _add(result, point)
        point = _add(point, point)
        s >>= 1
    return result


def _equal(a, b):
    return (a[0] * b[2] - b[0] * a[2]) % P == 0 and (a[1] * b[2] - b[1] * a[2]) % P == 0


def _recover_x(y, sign):
    if y >= P:
        return None
    x2 = (y * y - 1) * _inv(D * y * y + 1)
    if x2 == 0:
        return None if sign else 0
    x = pow(x2, (P + 3) // 8, P)
    if (x * x - x2) % P != 0:
        x = x * SQRT_M1 % P
    if (x * x - x2) % P != 0:
        return None
    if (x & 1) != sign:
        x = P - x
    return x


_GY = 4 * _inv(5) % P
_GX = _recover_x(_GY, 0)
G = (_GX, _GY, 1, _GX * _GY % P)


def _compress(point):
    zinv = _inv(point[2])
    x = point[0] * zinv % P
    y = point[1] * zinv % P
    return int.to_bytes(y | ((x & 1) << 255), 32, "little")


def _decompress(data):
    if len(data) != 32:
        return None
    y = int.from_bytes(data, "little")
    sign = y >> 255
    y &= (1 << 255) - 1
    x = _recover_x(y, sign)
    return None if x is None else (x, y, 1, x * y % P)


def _hash_int(data):
    return int.from_bytes(hashlib.sha512(data).digest(), "little")


class Key:
    """A private key: the scalar and the prefix RFC 8032 signs with."""

    def __init__(self, scalar, prefix):
        self.scalar = scalar
        self.prefix = prefix
        self.public = _compress(_mul(scalar, G))

    @classmethod
    def from_seed(cls, seed):
        h = hashlib.sha512(seed).digest()
        a = int.from_bytes(h[:32], "little")
        a &= (1 << 254) - 8
        a |= 1 << 254
        return cls(a, h[32:])

    @classmethod
    def parse(cls, text):
        """Sparkle's exported key: base64 of a 32-byte seed, or of an older
        Sparkle's 96 bytes (the expanded key, then the public key)."""
        try:
            raw = base64.b64decode("".join(text.split()), validate=True)
        except ValueError:
            raise SystemExit("sparkle.py: the private key isn't base64") from None
        if len(raw) == 32:
            return cls.from_seed(raw)
        if len(raw) in (64, 96):
            key = cls(int.from_bytes(raw[:32], "little"), raw[32:64])
            if len(raw) == 96 and raw[64:] != key.public:
                raise SystemExit("sparkle.py: the private key's two halves don't agree")
            return key
        raise SystemExit(
            f"sparkle.py: a private key is 32 bytes (or an older Sparkle's 96), not {len(raw)}"
        )

    def sign(self, message):
        r = _hash_int(self.prefix + message) % Q
        big_r = _compress(_mul(r, G))
        h = _hash_int(big_r + self.public + message) % Q
        s = (r + h * self.scalar) % Q
        return big_r + int.to_bytes(s, 32, "little")


def verify(public, message, signature):
    if len(public) != 32 or len(signature) != 64:
        return False
    a = _decompress(public)
    r = _decompress(signature[:32])
    if a is None or r is None:
        return False
    s = int.from_bytes(signature[32:], "little")
    if s >= Q:
        return False
    h = _hash_int(signature[:32] + public + message) % Q
    return _equal(_mul(s, G), _add(r, _mul(h, a)))


def b64(data):
    return base64.b64encode(data).decode()


def private_key():
    text = os.environ.get("SPARKLE_PRIVATE_KEY", "")
    if not text.strip():
        raise SystemExit("sparkle.py: set SPARKLE_PRIVATE_KEY to the private key, base64")
    return Key.parse(text)


SPARKLE_NS = "http://www.andymatuschak.org/xml-namespaces/sparkle"


def appcast(version, url, length, signature, minimum_system="26.0", release_page=None, date=None):
    """One release, as Sparkle reads it: the version both as the build
    (sparkle:version, CFBundleVersion) and for people, and the DMG with its
    size and signature."""
    date = date or email.utils.formatdate(usegmt=True)
    link = f"\n      <link>{escape(release_page)}</link>" if release_page else ""
    return f"""<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="{SPARKLE_NS}">
  <channel>
    <title>VibeOS</title>
    <item>
      <title>VibeOS {escape(version)}</title>
      <pubDate>{escape(date)}</pubDate>{link}
      <sparkle:version>{escape(version)}</sparkle:version>
      <sparkle:shortVersionString>{escape(version)}</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>{escape(minimum_system)}</sparkle:minimumSystemVersion>
      <enclosure url={quoteattr(url)} length="{int(length)}" type="application/octet-stream" sparkle:edSignature={quoteattr(signature)}/>
    </item>
  </channel>
</rss>
"""


def read_appcast(text):
    """What Sparkle takes from the appcast: the newest item's fields."""
    root = ET.fromstring(text)
    item = root.find("channel/item")
    enclosure = item.find("enclosure")
    sp = "{%s}" % SPARKLE_NS
    return {
        "version": item.findtext(sp + "version"),
        "short": item.findtext(sp + "shortVersionString"),
        "minimum": item.findtext(sp + "minimumSystemVersion"),
        "url": enclosure.get("url"),
        "length": int(enclosure.get("length")),
        "signature": enclosure.get(sp + "edSignature"),
    }


# RFC 8032, section 7.1: secret key, public key, message, signature.
VECTORS = [
    (
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        "",
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155"
        "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    ),
    (
        "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
        "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        "72",
        "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da"
        "085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    ),
    (
        "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
        "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        "af82",
        "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac"
        "18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    ),
]


def self_test():
    for secret, public, message, signature in VECTORS:
        key = Key.from_seed(bytes.fromhex(secret))
        msg = bytes.fromhex(message)
        assert key.public.hex() == public, "public key"
        assert key.sign(msg).hex() == signature, "signature"
        assert verify(key.public, msg, bytes.fromhex(signature)), "verify"
        assert not verify(key.public, msg + b"x", bytes.fromhex(signature)), "a changed message"
    # Every form Sparkle exports reads to the same key.
    seed = bytes.fromhex(VECTORS[0][0])
    key = Key.from_seed(seed)
    expanded = int.to_bytes(key.scalar, 32, "little") + key.prefix
    assert Key.parse(b64(seed)).public == key.public
    assert Key.parse(b64(expanded)).public == key.public
    old = Key.parse(b64(expanded + key.public))
    assert old.public == key.public and old.sign(b"dmg") == key.sign(b"dmg")
    for bad in [b64(b"short"), "not base64!", b64(expanded + bytes(32))]:
        try:
            Key.parse(bad)
        except SystemExit:
            pass
        else:
            raise AssertionError("a bad key was taken")
    # A new key signs, and only its own signature verifies.
    fresh = Key.from_seed(os.urandom(32))
    data = os.urandom(4096)
    sig = fresh.sign(data)
    assert verify(fresh.public, data, sig)
    assert not verify(key.public, data, sig)
    assert not verify(fresh.public, data, sig[:-1] + bytes([sig[-1] ^ 1]))
    # Against an independent implementation, where there is one.
    try:
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    except ImportError:
        print("sparkle.py: cryptography isn't installed; RFC 8032's vectors alone")
    else:
        seed = os.urandom(32)
        theirs = Ed25519PrivateKey.from_private_bytes(seed)
        ours = Key.from_seed(seed)
        raw = theirs.public_key().public_bytes_raw()
        assert raw == ours.public, "public key differs from cryptography's"
        assert theirs.sign(data) == ours.sign(data), "signature differs from cryptography's"
        theirs.public_key().verify(ours.sign(data), data)
        print("sparkle.py: agrees with the cryptography package")
    # The appcast parses, and says what it was given, escaped.
    url = "https://github.com/o/r/releases/download/v1.2.3/VibeOS-1.2.3.dmg?a=1&b=2"
    text = appcast("1.2.3", url, 1234, b64(sig), release_page="https://example.com/r?a&b")
    got = read_appcast(text)
    assert got == {
        "version": "1.2.3",
        "short": "1.2.3",
        "minimum": "26.0",
        "url": url,
        "length": 1234,
        "signature": b64(sig),
    }, got
    print("sparkle.py: self-test passed")


def main(argv):
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__.strip())
        return 0
    cmd, args = argv[0], argv[1:]
    if cmd == "keygen":
        print(b64(os.urandom(32)))
    elif cmd == "public-key":
        print(b64(private_key().public))
    elif cmd == "sign" and len(args) == 1:
        with open(args[0], "rb") as f:
            print(b64(private_key().sign(f.read())))
    elif cmd == "verify" and len(args) == 3:
        with open(args[0], "rb") as f:
            data = f.read()
        try:
            sig, pub = base64.b64decode(args[1]), base64.b64decode(args[2])
        except ValueError:
            return 1
        return 0 if verify(pub, data, sig) else 1
    elif cmd == "appcast":
        opts = dict(zip(args[::2], args[1::2]))
        need = ["--version", "--url", "--file", "--signature"]
        missing = [n for n in need if n not in opts]
        if missing or len(args) % 2:
            raise SystemExit("sparkle.py appcast: needs " + " ".join(f"{n} …" for n in need))
        sys.stdout.write(
            appcast(
                opts["--version"],
                opts["--url"],
                os.path.getsize(opts["--file"]),
                opts["--signature"],
                opts.get("--minimum-system", "26.0"),
                opts.get("--release-page"),
            )
        )
    elif cmd == "self-test":
        self_test()
    else:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
