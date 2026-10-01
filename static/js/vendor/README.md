Third-party browser libraries, vendored unmodified (apart from dropping source-map comments) so
rbb stays a single self-contained binary.

| File | Version | License | Source |
|---|---|---|---|
| `openpgp.min.mjs` | OpenPGP.js 6.3.2 | LGPL-3.0-or-later | https://github.com/openpgpjs/openpgpjs (`dist/openpgp.min.mjs`) |
| `qrcode.mjs` | qrcode-generator 2.0.4 | MIT | https://github.com/kazuhikoarase/qrcode-generator (`dist/qrcode.mjs`) |

To update: `npm pack openpgp@<version>` / `npm pack qrcode-generator@<version>`, copy the files above,
and bump the `?v=` query strings in `static/js/pgp.mjs` and the templates that load it.
