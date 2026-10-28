# Vendored IndexedDB libraries (test fixtures)

The compatibility proof for redjs's IndexedDB (m25 indexeddb-design §8.3):
`tests/idb_libs.rs` loads these dists UNMODIFIED into a redjs context and runs
the scenarios each library's users depend on. Test fixtures only — nothing here
is compiled into redjs or shipped with any build.

| Folder | Package | Version | File (from the npm tarball) | License | sha256 |
|---|---|---|---|---|---|
| `dexie-4.4.7/` | [dexie](https://www.npmjs.com/package/dexie) | 4.4.7 | `dist/dexie.js` | Apache-2.0 (`LICENSE`, `NOTICE`) | `5245aa68d9f410c4b20030b1fbdca370f2cdb61d24c2494d56c7da541334eda3` |
| `idb-8.0.4/` | [idb](https://www.npmjs.com/package/idb) | 8.0.4 | `build/umd.js` | ISC (`LICENSE`) | `84a1b83a821969a36592964874997757d8bde798dc1b89a5891391bd29e5c08c` |
| `idb-keyval-6.3.0/` | [idb-keyval](https://www.npmjs.com/package/idb-keyval) | 6.3.0 | `dist/umd.js` | Apache-2.0 (`LICENCE`) | `1d5aefa0c1cc78962ef116f11b7608dafeabf4950f71cb99fdab410422921d52` |
| `localforage-1.10.0/` | [localforage](https://www.npmjs.com/package/localforage) | 1.10.0 | `dist/localforage.js` | Apache-2.0 (`LICENSE`) | `e9f100dee1745c2b62b08705672e474d3e7c5e871763b2d7ec951f1222ba5b96` |

Fetched from `https://registry.npmjs.org/<name>/-/<name>-<version>.tgz` on
2026-10-28. To update: replace the file from the new tarball, update the
version, folder name and hash here, and re-run `cargo test --test idb_libs`.
