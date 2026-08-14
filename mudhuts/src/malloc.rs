//! Works around a real glibc allocator behavior, not a mudhuts-side leak —
//! same fix [COSMIC applies in `libcosmic`](https://github.com/pop-os/libcosmic/blob/master/src/malloc.rs),
//! adopted here after using it to explain the same symptom mudhuts hit.
//!
//! Diagnosed on a live, long-running `mudhuts --tty` session via
//! `malloc_info()` (called through `gdb`, redirected into an in-process
//! `open_memstream` buffer since the process's own stderr is the real
//! console — `/dev/tty1` — not somewhere safe to write to): glibc's malloc
//! had reserved ~850MB of address space (`system`) from the OS, but only
//! ~22MB of that was actually in use — the rest was sitting idle, spread
//! across several separate per-thread arenas (`malloc_info`'s own term for
//! one is `<heap>`) that a first attempt at fixing this
//! (`mallopt(M_ARENA_MAX, 1)`, forcing every thread onto one shared arena)
//! addressed by brute force rather than by fixing the actual mechanism.
//!
//! The real mechanism, and the reason COSMIC's own fix works without
//! forcing single-arena serialization: glibc's `M_MMAP_THRESHOLD` — the
//! size above which an allocation is satisfied via its own individual
//! `mmap` (trivially `munmap`-able the instant it's freed) rather than
//! carved out of the growable brk-heap (only reclaimable from the *top*,
//! and only if nothing above it is still live) — isn't a fixed value by
//! default. glibc raises it dynamically whenever it sees an allocation
//! freed that was itself above the current threshold, on the theory that
//! the program's "typical large allocation size" just grew. That adaptive
//! behavior is exactly what lets a handful of transiently large
//! allocations (an offscreen composite buffer resized once for a bigger
//! output, a large PTY read, a resized terminal grid) ratchet the
//! threshold up permanently, after which *smaller* allocations that used
//! to go straight to individually-freeable `mmap` start landing on the
//! brk-heap instead — where they can strand everything below them for the
//! rest of the process's life the moment anything above them is still
//! referenced.
//!
//! [`limit_mmap_threshold`] pins the threshold to a small, static value
//! instead, called once at startup before that ratcheting can happen at
//! all. [`trim`] (`malloc_trim`, called once per render pass — see
//! `render::build_frame_elements`'s call to it) actively invites glibc to
//! hand back whatever's genuinely free at the top of the main arena on a
//! cadence tied to real application activity, rather than leaving it
//! purely to glibc's own internal heuristics (which is what made the
//! earlier ad-hoc, one-off `malloc_trim(0)` call during diagnosis barely
//! move `RSS` at all — nothing was inviting it to happen regularly).

use std::os::raw::c_int;

const M_MMAP_THRESHOLD: c_int = -3;

unsafe extern "C" {
    fn malloc_trim(pad: usize);
    fn mallopt(param: c_int, value: c_int) -> c_int;
}

/// Ask glibc to hand back whatever's currently free at the top of the main
/// arena. Cheap to call when there's nothing to give back (the common
/// case) — safe to call every render pass rather than rate-limiting it.
#[inline]
pub fn trim(pad: usize) {
    unsafe {
        malloc_trim(pad);
    }
}

/// Pin glibc's `mmap` threshold to `threshold` bytes, instead of letting it
/// grow on its own — see this module's doc comment for why the *adaptive*
/// default is the actual mechanism behind glibc hoarding memory here, not
/// just "fragmentation" in the abstract. Must run before anything else has
/// a chance to allocate something large enough to start that ratchet —
/// call this first in `main`, same as `App::run` does in `libcosmic`.
#[inline]
pub fn limit_mmap_threshold(threshold: i32) {
    unsafe {
        mallopt(M_MMAP_THRESHOLD, threshold as c_int);
    }
}
