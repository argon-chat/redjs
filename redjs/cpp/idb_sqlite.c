/*
 * IndexedDB's SQLite (m25 P2, research/reports/m25/indexeddb-design.md §2.2).
 *
 * ONE translation unit: the SQLite3 Multiple Ciphers amalgamation (MIT;
 * SQLite itself is public domain), fetched and hash-verified by setup.ps1 at
 * the version pinned in ../pins.txt, compiled by build.rs only with the
 * `indexeddb-sqlite` feature — plus the handful of `rjs_sql_*` wrappers below,
 * which are the whole of what Rust sees (src/webapi/indexeddb/sqlite.rs).
 *
 * No `sqlite3_*` symbol leaves this object. SQLITE3MC_USE_DISPATCH_TABLE is
 * sqlite3mc's own switch for exactly that: every SQLITE_API function becomes
 * `static`, and the only external names are three dispatch-table pointers,
 * renamed here to `redjs_sqlite3mc_api_*` (and NOT dllexport'ed). An embedder
 * that links its own SQLite next to redjs therefore links two disjoint copies,
 * never a duplicate-symbol clash or — worse — one silently binding to the other.
 * (sqlite3mc's caveat stands: two SQLite copies in one process must not open
 * the same database FILE. Ours only ever opens files under the IndexedDB
 * folder, and the folder lock (sqlite.rs) keeps every other process out.)
 */

/* --- what is compiled ------------------------------------------------------ */

#define SQLITE3MC_USE_DISPATCH_TABLE 1
#define SQLITE3MC_API_TABLE_PREFIX redjs_sqlite3mc
#define SQLITE3MC_DISPATCH_API /* not __declspec(dllexport): nothing to export */

/* Connections are only ever used by the thread that owns them (the IndexedDB
   server thread; a unit test's own thread). Mode 2 = no per-connection mutex. */
#define SQLITE_THREADSAFE 2
#define SQLITE_DQS 0
#define SQLITE_DEFAULT_MEMSTATUS 0
#define SQLITE_DEFAULT_WAL_SYNCHRONOUS 1 /* NORMAL in WAL; `strict` asks for FULL per transaction */
#define SQLITE_LIKE_DOESNT_MATCH_BLOBS 1
#define SQLITE_MAX_EXPR_DEPTH 0
#define SQLITE_USE_ALLOCA 1
/* Temporary tables, sorts and transient indices are NOT encrypted by the
   codec, so they never touch the disk (sqlite3mc's own default, stated). */
#define SQLITE_TEMP_STORE 3

/* --- the trim (owner: binary weight matters) --------------------------------
   `RJS_SQL_UNTRIMMED` exists only to measure what the trim saves.

   One cipher: ChaCha20-Poly1305, sqlite3mc's default. The others are
   compiled out — no database of ours can ever be in them.

   Omitted: only options that remove an API or a built-in FUNCTION, never one
   that changes the SQL grammar (the amalgamation's parser is pre-generated,
   so grammar-level SQLITE_OMIT_* — triggers, views, CTEs, window functions,
   ALTER, virtual tables — would leave a parser that disagrees with the code;
   SQLite documents those as unsupported for the amalgamation). Every SQL
   statement IndexedDB runs is in sqlite.rs; none of it uses anything below.
   FTS3/4/5, RTREE, GEOPOLY, the math and the session/extension modules are
   OFF by default in this amalgamation and stay off. */
#ifndef RJS_SQL_UNTRIMMED
#define HAVE_CIPHER_AES_128_CBC 0
#define HAVE_CIPHER_AES_256_CBC 0
#define HAVE_CIPHER_CHACHA20 1
#define HAVE_CIPHER_SQLCIPHER 0
#define HAVE_CIPHER_RC4 0
#define HAVE_CIPHER_ASCON128 0
#define HAVE_CIPHER_AEGIS 0
#define SQLITE_OMIT_DEPRECATED 1
#define SQLITE_OMIT_SHARED_CACHE 1
#define SQLITE_OMIT_LOAD_EXTENSION 1
/* ...whose routine table the dispatch-table mode still names (378504 in
   2.6.1): with no loadable extensions there is no table, so its pointer is
   null. Nothing calls through it. */
#define sqlite3Apis (*(const sqlite3_api_routines *)0)
#define SQLITE_OMIT_PROGRESS_CALLBACK 1
#define SQLITE_OMIT_DECLTYPE 1
#define SQLITE_OMIT_JSON 1
#define SQLITE_OMIT_UTF16 1
#define SQLITE_OMIT_COMPLETE 1
#define SQLITE_OMIT_GET_TABLE 1
#define SQLITE_OMIT_TRACE 1
#define SQLITE_OMIT_AUTHORIZATION 1
#define SQLITE_OMIT_INCRBLOB 1
#define SQLITE_OMIT_DATETIME_FUNCS 1
#define SQLITE_OMIT_COMPILEOPTION_DIAGS 1
#define SQLITE_OMIT_INTROSPECTION_PRAGMAS 1
#define SQLITE_OMIT_DESERIALIZE 1
#define SQLITE_OMIT_TCL_VARIABLE 1
#define SQLITE_OMIT_AUTOMATIC_INDEX 1
#define SQLITE_OMIT_AUTOVACUUM 1
#define SQLITE_UNTESTABLE 1
#endif

/* The bundled libsodium ChaCha20/Poly1305 code keeps libsodium's own public
   names, which an embedder linking the real libsodium would collide with. */
#define crypto_onetimeauth_poly1305_donna_implementation redjs_mc_poly1305_donna_implementation
#define crypto_onetimeauth_poly1305_sse2_implementation redjs_mc_poly1305_sse2_implementation
#define crypto_stream_chacha20_dolbeau_avx2_implementation redjs_mc_chacha20_dolbeau_avx2_implementation
#define crypto_stream_chacha20_dolbeau_avx512_implementation redjs_mc_chacha20_dolbeau_avx512_implementation
#define crypto_stream_chacha20_dolbeau_ssse3_implementation redjs_mc_chacha20_dolbeau_ssse3_implementation
#define crypto_stream_chacha20_ref_implementation redjs_mc_chacha20_ref_implementation
#define crypto_stream_chacha20_ietf_ext_xor_ic redjs_mc_chacha20_ietf_ext_xor_ic
#define crypto_stream_chacha20_ietf_xor_ic redjs_mc_chacha20_ietf_xor_ic
#define crypto_stream_chacha20_xor_ic redjs_mc_chacha20_xor_ic
#define crypto_stream_chacha20_pick_best_implementation redjs_mc_chacha20_pick_best_implementation
#define sqlite3mcChaCha20HwAccelerated redjs_mc_chacha20_hw_accelerated
#define sqlite3mcChaCha20HwConfig redjs_mc_chacha20_hw_config

/* Two upstream quirks under clang-cl (2.6.1), neither of which touches a path
   we execute differently:
   - libsodium's poly1305-donna64 defines its 128-bit helpers only for
     `!_MSC_VER` (native __int128) or for MSVC WITHOUT __int128 (a struct).
     clang-cl is MSVC-flavoured AND has __int128, so it got neither; these are
     the `!_MSC_VER` definitions, which is exactly the type clang-cl picked.
   - the dispatch table lists sqlite3_win32_set_directory8/16 in swapped
     slots. It is a table we never call through (the wrappers below call the
     functions directly); MSVC warns about it, clang 20 errors by default. */
#if defined(__clang__) && defined(_MSC_VER) && defined(__SIZEOF_INT128__)
#define SODIUM_MUL(out, x, y) out = ((uint128_t)x * y)
#define SODIUM_ADD(out, in) out += in
#define SODIUM_ADDLO(out, in) out += in
#define SODIUM_SHR(in, shift) (unsigned long long)(in >> (shift))
#define SODIUM_LO(in) (unsigned long long)(in)
#endif
#if defined(__clang__)
#pragma clang diagnostic ignored "-Wincompatible-function-pointer-types"
#endif

#include "sqlite3mc_amalgamation.c"

#ifdef SQLITE_OMIT_LOAD_EXTENSION
/* sqlite3mc's core dispatch table names this one unconditionally; with the
   loader compiled out, it is the refusal SQLite itself would give. */
static int sqlite3_enable_load_extension(sqlite3 *db, int onoff) {
  (void)db;
  (void)onoff;
  return SQLITE_ERROR;
}
#endif

/* --- the surface Rust sees ------------------------------------------------- */

typedef long long rjs_i64;

int rjs_sql_open(const char *path_utf8, sqlite3 **out) {
  return sqlite3_open_v2(path_utf8, out,
                         SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_NOMUTEX | SQLITE_OPEN_EXRESCODE,
                         0);
}
int rjs_sql_close(sqlite3 *db) { return sqlite3_close(db); }
int rjs_sql_exec(sqlite3 *db, const char *sql) { return sqlite3_exec(db, sql, 0, 0, 0); }
const char *rjs_sql_errmsg(sqlite3 *db) { return sqlite3_errmsg(db); }
int rjs_sql_errcode(sqlite3 *db) { return sqlite3_extended_errcode(db); }
const char *rjs_sql_errstr(int rc) { return sqlite3_errstr(rc); }
int rjs_sql_changes(sqlite3 *db) { return sqlite3_changes(db); }
int rjs_sql_autocommit(sqlite3 *db) { return sqlite3_get_autocommit(db); }
const char *rjs_sql_version(void) { return sqlite3_libversion(); }
const char *rjs_sql_mc_version(void) { return sqlite3mc_version(); }

int rjs_sql_prepare(sqlite3 *db, const char *sql, int n, sqlite3_stmt **out) {
  return sqlite3_prepare_v3(db, sql, n, SQLITE_PREPARE_PERSISTENT, out, 0);
}
int rjs_sql_step(sqlite3_stmt *s) { return sqlite3_step(s); }
int rjs_sql_reset(sqlite3_stmt *s) { return sqlite3_reset(s); }
int rjs_sql_clear_bindings(sqlite3_stmt *s) { return sqlite3_clear_bindings(s); }
int rjs_sql_finalize(sqlite3_stmt *s) { return sqlite3_finalize(s); }

/* SQLITE_STATIC: the caller keeps the bytes alive until the statement is
   reset (sqlite.rs binds, steps and resets in one call). A zero-length
   binding is a zero-length BLOB, never NULL. */
int rjs_sql_bind_blob(sqlite3_stmt *s, int i, const void *p, int n) {
  static const unsigned char empty = 0; /* NULL would bind SQL NULL */
  return sqlite3_bind_blob(s, i, n == 0 ? (const void *)&empty : p, n, SQLITE_STATIC);
}
int rjs_sql_bind_i64(sqlite3_stmt *s, int i, rjs_i64 v) { return sqlite3_bind_int64(s, i, v); }
int rjs_sql_bind_f64(sqlite3_stmt *s, int i, double v) { return sqlite3_bind_double(s, i, v); }

int rjs_sql_column_type(sqlite3_stmt *s, int i) { return sqlite3_column_type(s, i); }
const void *rjs_sql_column_blob(sqlite3_stmt *s, int i) { return sqlite3_column_blob(s, i); }
int rjs_sql_column_bytes(sqlite3_stmt *s, int i) { return sqlite3_column_bytes(s, i); }
rjs_i64 rjs_sql_column_i64(sqlite3_stmt *s, int i) { return sqlite3_column_int64(s, i); }
double rjs_sql_column_f64(sqlite3_stmt *s, int i) { return sqlite3_column_double(s, i); }

/* IDB_CIPHER_V1 (design §5.5) — part of the on-disk format. ChaCha20-Poly1305
   with sqlite3mc's current (non-legacy) page layout, no plaintext header, and
   a RAW 256-bit key (`raw:` + 64 hex digits bypasses the KDF: the key is 32
   uniformly random bytes, so PBKDF2 stretching would buy nothing and cost
   every open ~64 k rounds). Set on the connection before keying. */
static int rjs_sql_cipher_v1(sqlite3 *db) {
  int idx = sqlite3mc_cipher_index("chacha20");
  if (idx <= 0) return SQLITE_ERROR;
  if (sqlite3mc_config(db, "cipher", idx) != idx) return SQLITE_ERROR;
  if (sqlite3mc_config_cipher(db, "chacha20", "legacy", 0) != 0) return SQLITE_ERROR;
  if (sqlite3mc_config_cipher(db, "chacha20", "plaintext_header_size", 0) != 0) return SQLITE_ERROR;
  return SQLITE_OK;
}

static void rjs_sql_raw_key(const unsigned char key[32], char out[4 + 64]) {
  static const char hex[] = "0123456789abcdef";
  memcpy(out, "raw:", 4);
  for (int i = 0; i < 32; i++) {
    out[4 + 2 * i] = hex[key[i] >> 4];
    out[4 + 2 * i + 1] = hex[key[i] & 15];
  }
}

/* Opens the codec with `key` (32 bytes). */
int rjs_sql_key(sqlite3 *db, const unsigned char *key) {
  char raw[4 + 64];
  int rc = rjs_sql_cipher_v1(db);
  if (rc != SQLITE_OK) return rc;
  rjs_sql_raw_key(key, raw);
  rc = sqlite3_key_v2(db, "main", raw, (int)sizeof raw);
  sqlite3mcSecureZeroMemory(raw, sizeof raw);
  return rc;
}

/* Re-encrypts the whole file under `key`, or decrypts it (key == NULL).
   sqlite3mc rewrites every page; plaintext -> encrypted is refused in WAL
   mode, so the caller switches the journal mode around this. */
int rjs_sql_rekey(sqlite3 *db, const unsigned char *key) {
  char raw[4 + 64];
  int rc;
  if (key == 0) return sqlite3_rekey_v2(db, "main", 0, 0);
  rc = rjs_sql_cipher_v1(db);
  if (rc != SQLITE_OK) return rc;
  rjs_sql_raw_key(key, raw);
  rc = sqlite3_rekey_v2(db, "main", raw, (int)sizeof raw);
  sqlite3mcSecureZeroMemory(raw, sizeof raw);
  return rc;
}
