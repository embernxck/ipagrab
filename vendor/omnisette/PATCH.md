# omnisette — copy with verified TLS

Source: Impactor (https://github.com/khcrysalis/Impactor) rev `b13d99559febd63e3710dfdd0ef8cdd78d10ae13`,
`3rdparty/omnisette` (MIT, see `LICENSE`). Used via `[patch."https://github.com/khcrysalis/Impactor"]`.

Upstream builds its HTTP clients with `danger_accept_invalid_certs(true)`, so the anisette server and
Apple's provisioning endpoint were reached without certificate checks. Changes:

- `src/adi_proxy.rs`, `src/remote_anisette_v3.rs`: no `danger_accept_invalid_certs`; normal verification
  plus Apple Root CA (`src/apple_root.der`, same file `plume_core` ships).
- `src/lib.rs`: `APPLE_ROOT`, `#![allow(warnings)]`.
- `Cargo.toml`: workspace-inherited fields written out.
