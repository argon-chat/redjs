# Native AOT memory profiles

The native image loader has an explicit, runtime-only capacity profile. Existing callers of `load_image(path, identity)` and the original `rjs_aot_load_file` C symbol retain the `Default` policy. Neither profile changes the image identity, pinned frontend, builtin-table digest, narrow reference ABI or compile options.

```rust,ignore
use redjs::aot::{self, MemoryProfile};
// Perform the normal trusted artifact / exact ABI manifest checks first.
unsafe { aot::load_image_with_profile(path, identity, MemoryProfile::Compact)?; }
let context = redjs::api::Context::new();
```

Select the profile on the VM thread before any JSC initialization. A profile cannot be changed after initialization, and accepting an AOT image cannot fall back to a JIT context in that process. Invalid C profile values and conflicting effective options fail explicitly.

| Profile | `structureHeapSizeInKB` | `markedBlockCageSizeInMB` |
| --- | ---: | ---: |
| Default | Engine defaults / existing external overrides | Engine defaults / existing external overrides |
| Compact | 262144 | 1024 |

Compact gives the Structure heap a 256 MiB capacity and the narrow-reference cell cage a 1 GiB address range. These are engine capacities, not a total process RAM limit or a working-set target. Large workloads can exhaust those capacities sooner. Applications must qualify their workload; ordinary JIT processes receive no new limits. The loader does not patch the prebuilt engine or remove its required committed tables.

`memory_settings()` reports the selected profile, actual configured option values and the current protection of the initial image view, without initializing JSC. A Default structure-heap value of zero means engine automatic sizing; it does not mean no memory is allocated.

The initial image view uses `PAGE_READONLY` / `FILE_MAP_READ` on Windows and `mmap(PROT_READ, MAP_PRIVATE)` on Unix. The engine receives the retained file handle or descriptor and still creates its own executable mappings and writable private clones. Its initial view and file remain alive for the accepted image's lifetime. Windows denies write sharing; Unix callers must keep the published inode immutable for that lifetime.

Hosts that verify an image must pass the verified file object to
`load_image_file_with_profile(&File, identity, profile)`, not hash one file and
later reopen its path. On Windows, open with `GENERIC_READ | GENERIC_EXECUTE`
and `FILE_SHARE_READ | FILE_SHARE_DELETE` (deny write sharing). Hash and load
that same `File`. The loader duplicates the HANDLE with the same access and
sharing semantics; the caller may drop its `File` after success. Atomic path
replacement or symlink retargeting then cannot replace the file object being
executed. The deny-write share remains for the native image's lifetime. Files
opened with unrestricted write sharing do not satisfy this unsafe API's
immutability contract.

The common native loader owns its file and read-only view through RAII. It
validates the image before reading its header and requires the cell cage
before publishing the image, so rejected images release their duplicate,
view and mapping. Only an accepted image retains those resources. Engine
native-helper registration uses `std::call_once`: the pinned engine asserts
if those bindings are registered a second time. A rejected image can now be
retried without rebinding the one-time registry.

`digest_reader(impl Read)` validates SHA-256 using a fixed 16 KiB stack buffer, retrying interrupted reads and propagating other I/O errors. Hosts can hash their pinned read handle without allocating a second image-sized byte vector. This checksum is a compatibility/integrity check, not an authenticity signature.

## Focused verification

The `aot-image` example accepts `run-web IMAGE [default|compact] [switch]`. It checks effective options and the real view protection with `VirtualQuery`, then executes the image with Rust callbacks, GC, structuredClone, URL, Event, timers/Promises and builtin/object-brand checks. `switch` also verifies that a conflicting profile is rejected without changing the running image. `reject-image IMAGE [default|compact]` and `reject-late-profile IMAGE` are isolated-process negative probes.

Use the exact matching engine/frontend/table when building the example. Run each profile in a fresh process; do not put both initialization paths into a shared unit-test VM. The unit tests separately cover bounded streaming reads, interrupted reads, I/O errors, invalid C profile values and the original C loader symbol.

The separate `run-stress IMAGE [default|compact]` fixture allocates 100,000
distinct dynamic-shape objects in four rounds, with at most 25,000 active rows
and 128 retained samples. It checks property updates/deletions, arrays, Map,
WeakRef and 61 explicit GC/compiled-JS reentries from Rust. This bounded
fixture does not qualify production Argon workloads.

`run-verified IMAGE [default|compact]` retargets an actual Windows symlink
after hashing its pinned file, loads that same file handle, drops the caller's
File and executes the existing native Web API/GC probe. `reject-handles
CORRUPT_IMAGE` repeats invalid-profile/handle and corrupt-file rejection 1,000
times, compares process kernel-handle counts and checks that the caller's
handle remains usable. Both are isolated-process diagnostics.
