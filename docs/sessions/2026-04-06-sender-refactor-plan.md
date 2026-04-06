# Session Summary: SenderAgent Refactor Plan (Step 3)

**Date:** 2026-04-06  
**Duration:** ~1 interaction  
**Focus Area:** agent/sender.rs  
**Prerequisites:** Step 1 (term_buffer.rs) and Step 2 (network_publication.rs) complete

## Objectives

- [x] Analyze current SenderAgent / PublicationEntry architecture
- [x] Identify borrow-checker constraints for sender_scan + send_data
- [x] Design PublicationEntry refactor (own NetworkPublication)
- [x] Design do_send() scan+send integration
- [x] Plan add_publication() API change
- [x] Identify test cases
- [ ] Implement sender.rs refactor (deferred - plan only)

## Analysis

### Current PublicationEntry (to be replaced)

```rust
struct PublicationEntry {
    session_id: i32,       // -> publication.session_id()
    stream_id: i32,        // -> publication.stream_id()
    term_id: i32,          // -> publication.active_term_id()
    term_offset: i32,      // -> publication.term_offset()
    term_length: i32,      // -> publication.term_length()
    mtu: i32,              // -> publication.mtu()
    endpoint_idx: usize,   // KEEP
    dest_addr: Option<..>, // KEEP
    needs_setup: bool,     // KEEP
    time_of_last_send_ns: i64,  // KEEP
    time_of_last_setup_ns: i64, // KEEP
}
```

6 scalar fields become `publication: NetworkPublication`. 5 fields remain.

### do_send() Borrow-Checker Challenge (CRITICAL)

The main difficulty: sender_scan's emit callback needs to call `send_data` on an endpoint, but both `publications[idx]` and `endpoints[ep_idx]` live inside `&mut self`.

Current code avoids this by copying ALL scalar fields into locals. With NetworkPublication, we need `&mut pub_entry.publication` for `sender_scan()` and `&self.endpoints[ep_idx]` for `send_data()` simultaneously.

#### Solution: Three-Phase Within Each Publication

```
Phase 1: Copy scalars (endpoint_idx, dest_addr, needs_setup, timers)
          Read publication accessors (session_id, stream_id, etc.)
          Use these for setup/heartbeat (borrows self.endpoints briefly)

Phase 2: Split borrow for scan+send
          &mut self.publications[idx].publication  -- for sender_scan
          &self.endpoints[ep_idx]                  -- for send_data (&self!)
          &mut self.poller                         -- for submit_send

Phase 3: Update timers on self.publications[idx]
```

Key insight: `send_data` takes `&self` (not `&mut self`), so we can hold both
`&mut publications[idx]` and `&endpoints[ep_idx]` simultaneously IF we avoid
overlapping with `&mut self`. This requires careful scoping.

**Concrete borrow strategy:**

```rust
// Phase 2: scan and send (split borrow block)
{
    let (pubs_left, pubs_right) = self.publications.split_at_mut(idx);
    let pub_entry = &mut pubs_right[0];
    let ep = &self.endpoints[ep_idx];  // immutable borrow OK
    let poller = &mut self.poller;
    let mtu_limit = pub_entry.publication.mtu();
    let dest = pub_entry.dest_addr.as_ref();

    let scanned = pub_entry.publication.sender_scan(mtu_limit, |_off, data| {
        let _ = ep.send_data(poller, data, dest);
    });

    if scanned > 0 {
        pub_entry.time_of_last_send_ns = now_ns;
    }
    bytes_sent += scanned as i32;
}
```

Wait - `split_at_mut` gives `(&mut [..idx], &mut [idx..])`. But `self.endpoints`
is a separate field. The issue is that `self.endpoints` and `self.publications`
are different fields - Rust allows borrowing different struct fields simultaneously.

Actually, the simpler approach works:

```rust
// The compiler allows borrowing disjoint fields of a struct.
// self.publications[idx] and self.endpoints[ep_idx] are different fields.
// But we access them through indexing, which borrows the whole Vec.
//
// Solution: take &mut to the specific element, then access other fields.
let pub_entry = &mut self.publications[idx];
let ep_idx = pub_entry.endpoint_idx;
// Can't do &self.endpoints[ep_idx] here - self already borrowed via publications!
```

The real solution is to use raw index access or restructure:

```rust
// Option A: pointer-based (unsafe but zero-cost)
// Option B: re-borrow after scope ends
// Option C: store scan results, then send in separate loop
```

**Recommended: Option C - two-phase scan-then-send**

Phase 1: For each publication, call sender_scan() and record frame boundaries
in a small stack buffer. Phase 2: For each recorded frame, call send_data().

But this requires knowing the frame data pointers across phases. The data lives
in the RawLog and remains valid as long as no new writes occur (single-threaded).

Actually, simplest correct approach:

```rust
// Borrow self.publications and self.endpoints as separate locals.
// Rust DOES allow this for direct struct field access:
let publications = &mut self.publications;
let endpoints = &self.endpoints;
let poller = &mut self.poller;

// But now publications and poller are separate borrows.
// In the loop, we borrow publications[idx] and endpoints[ep_idx] -
// these are disjoint collections, so the compiler allows it
// IF we don't go through &mut self.
```

This is the clean pattern:

```rust
fn do_send(&mut self, now_ns: i64) -> i32 {
    // ... round-robin setup ...

    // Destructure self into disjoint borrows.
    let publications = &mut self.publications;
    let endpoints = &mut self.endpoints;
    let poller = &mut self.poller;
    let heartbeat_interval_ns = self.heartbeat_interval_ns;

    let mut total_bytes = 0i32;
    let mut idx = start;

    for _ in 0..len {
        let pub_entry = &mut publications[idx];
        let ep_idx = pub_entry.endpoint_idx;

        // Read publication accessors (all Copy).
        let session_id = pub_entry.publication.session_id();
        let stream_id = pub_entry.publication.stream_id();
        // ... etc ...

        // Setup / heartbeat use &mut endpoints[ep_idx] + poller.
        // Need to drop pub_entry borrow first, or use separate scope.
        {
            let ep = &mut endpoints[ep_idx];
            // send_setup / send_heartbeat...
        }

        // Scan + send: pub_entry needs &mut, ep needs & (send_data is &self).
        {
            let pub_entry = &mut publications[idx];
            let ep = &endpoints[ep_idx];  // immutable ref for send_data
            let dest = pub_entry.dest_addr.as_ref();
            let limit = pub_entry.publication.mtu();

            let scanned = pub_entry.publication.sender_scan(limit, |_off, data| {
                let _ = ep.send_data(poller, data, dest);
            });
            // ...
        }

        idx += 1;
        if idx >= len { idx = 0; }
    }
    // ...
}
```

WAIT - `poller` is `&mut` and `ep.send_data(poller, ...)` takes `&mut P`.
So we need `&mut poller` inside the closure. But the closure also captures `ep`
as `&endpoints[ep_idx]`. And `pub_entry` is `&mut publications[idx]`.

These are all DISJOINT fields of `self` (publications, endpoints, poller).
Rust allows simultaneous mutable borrows of disjoint struct fields when accessed
directly (not through `&mut self`). Since we destructured at the top, this works.

But `poller` is already `&mut` and the closure captures it. Can a `FnMut` closure
capture `&mut poller`? Yes - the closure is `FnMut` and captures `poller` as
`&mut &mut UringTransportPoller`. Each call to the closure re-borrows it.

Hmm, actually there's a subtlety: `sender_scan` takes `FnMut(u32, &[u8])` and
the callback captures `poller: &mut UringTransportPoller`. During the scan, the
closure is called multiple times. Each call does `ep.send_data(poller, data, dest)`
which takes `&mut P` (poller). Between calls, the mutable borrow is released.

This should compile because:
1. `publications[idx]` and `endpoints[ep_idx]` are different Vec elements from
   different Vecs (disjoint fields after destructure)
2. `poller` is a third disjoint field
3. The closure captures `ep: &SendChannelEndpoint`, `poller: &mut UringTransportPoller`,
   and `dest: Option<&sockaddr_storage>` - all non-overlapping
4. `sender_scan` takes `&mut self.publication` which is `&mut publications[idx].publication`

This pattern compiles because we destructured `self` into separate field borrows.

### New PublicationEntry

```rust
struct PublicationEntry {
    publication: NetworkPublication,
    endpoint_idx: usize,
    dest_addr: Option<libc::sockaddr_storage>,
    needs_setup: bool,
    time_of_last_send_ns: i64,
    time_of_last_setup_ns: i64,
}
```

### New add_publication() Signature

```rust
pub fn add_publication(
    &mut self,
    endpoint_idx: usize,
    session_id: i32,
    stream_id: i32,
    initial_term_id: i32,
    term_length: u32,  // was i32
    mtu: u32,          // was i32
) -> Option<usize>     // was ()
```

Returns `Some(pub_idx)` on success, `None` if NetworkPublication::new fails.
The returned index can be used with `publication_mut(idx)` to call `offer()`.

### External Offer Access

```rust
/// Get mutable access to a publication for calling offer().
pub fn publication_mut(&mut self, idx: usize) -> Option<&mut NetworkPublication> {
    self.publications.get_mut(idx).map(|e| &mut e.publication)
}
```

### process_sm_and_nak() Changes

Minimal - just change field access pattern:

```
pub_entry.session_id     -> pub_entry.publication.session_id()
pub_entry.stream_id      -> pub_entry.publication.stream_id()
pub_entry.needs_setup     (unchanged - still on PublicationEntry)
```

### send_setup() Parameter Adaptation

Current endpoint API takes i32 for term fields (matching wire format).
NetworkPublication uses u32. Cast with `as i32` - safe for valid values.

```rust
ep.send_setup(
    poller,
    pub_entry.publication.session_id(),
    pub_entry.publication.stream_id(),
    pub_entry.publication.initial_term_id(),
    pub_entry.publication.active_term_id(),
    pub_entry.publication.term_offset() as i32,
    pub_entry.publication.term_length() as i32,
    pub_entry.publication.mtu() as i32,
    0, // ttl
    dest,
);
```

### send_heartbeat() Parameter Adaptation

```rust
ep.send_heartbeat(
    poller,
    pub_entry.publication.session_id(),
    pub_entry.publication.stream_id(),
    pub_entry.publication.active_term_id(),
    pub_entry.publication.term_offset() as i32,
    dest,
);
```

## Implementation Steps

### Step 3a: Modify PublicationEntry struct

1. Add `use crate::media::network_publication::NetworkPublication;` import
2. Replace scalar fields with `publication: NetworkPublication`
3. Keep endpoint_idx, dest_addr, needs_setup, timers

### Step 3b: Refactor add_publication()

1. Change term_length/mtu to u32
2. Return `Option<usize>`
3. Construct NetworkPublication::new() internally
4. Return None on failure, Some(idx) on success

### Step 3c: Add publication_mut() accessor

Simple delegating method for external offer() calls.

### Step 3d: Refactor do_send() - destructure + scan+send

1. Destructure self into disjoint field borrows at method start
2. Setup/heartbeat: read publication accessors, delegate to endpoint
3. Scan+send block: `pub_entry.publication.sender_scan(limit, |off, data| ep.send_data(poller, data, dest))`
4. Track scanned bytes, update time_of_last_send_ns
5. Accumulate total_bytes, return

### Step 3e: Refactor process_sm_and_nak()

Change `pub_entry.session_id` to `pub_entry.publication.session_id()` etc.
The drain_sm/drain_naks borrow pattern should still work since we access
publications through a separate loop.

NOTE: There's a borrow issue here too. Current code:
```rust
for ep in &mut self.endpoints {
    ep.drain_sm(|sm| {
        for pub_entry in &mut self.publications {  // ERROR: self already borrowed
```

This currently compiles because... actually, looking at the code again:
```rust
fn process_sm_and_nak(&mut self) {
    for ep in &mut self.endpoints {
        ep.drain_sm(|sm| {
            for pub_entry in &mut self.publications {
```

Wait, this should NOT compile - `self.endpoints` is borrowed by the outer loop,
and `self.publications` is borrowed inside the closure. But both go through `self`.

Actually, looking more carefully - the closure captures `self.publications`.
The outer loop iterates `self.endpoints`. These are disjoint fields.
Rust's borrow checker for closures does field-level tracking in some cases.

Hmm, but actually it might not track through `self` - closures capture entire
variables, not fields. Let me re-examine the current code... it does compile
currently (since the project builds), so Rust must be handling this correctly
through NLL/closure capture analysis.

With the refactor, the access changes from `pub_entry.session_id` to
`pub_entry.publication.session_id()`. Since `pub_entry` comes from iterating
`self.publications` which is already captured by the closure, this should
continue to work.

Actually wait - the current code has:
```rust
ep.drain_sm(|sm| {
    for pub_entry in &mut self.publications {
```

The closure captures `&mut self.publications`. The outer `for ep in &mut self.endpoints`
borrows `self.endpoints`. These are disjoint fields - Rust 2021 edition captures
individual fields in closures. This works.

### Step 3f: Update integration tests

1. Change `add_publication(ep_idx, 1001, 10, 0, 1 << 16, 1408)` to
   `add_publication(ep_idx, 1001, 10, 0, 1u32 << 16, 1408u32).expect("add pub")`
2. Add new test: offer data, run do_work, verify frames were sent (work_count > 0)

## Unit Tests Plan

| # | Test | What It Verifies |
|---|------|-----------------|
| 1 | Update `sender_with_endpoint_and_publication` | New API compiles, duty cycle runs with NetworkPublication |
| 2 | `sender_offer_and_send` (new) | offer() data, do_work() scans and sends, work_count > 0 |
| 3 | `sender_scan_produces_bytes_sent` (new) | After offer, do_send returns non-zero byte count |
| 4 | `sender_heartbeat_reads_from_publication` (new) | Heartbeat uses publication.active_term_id/term_offset |
| 5 | `sender_setup_reads_from_publication` (new) | Setup uses publication.initial_term_id/active_term_id |
| 6 | `sender_add_publication_returns_index` (new) | add_publication returns Some(0), Some(1), etc. |
| 7 | `sender_add_publication_invalid_returns_none` (new) | Bad term_length returns None |
| 8 | `sender_publication_mut_accessor` (new) | publication_mut(0) returns Some, publication_mut(999) returns None |

## Decisions Made

| Decision | Rationale |
|----------|-----------|
| Destructure self for disjoint field borrows | Clean Rust pattern, no unsafe. Allows simultaneous &mut publication + &endpoints + &mut poller |
| send_data takes &self (not &mut self) | Already the case. Enables immutable borrow of endpoint during scan closure |
| add_publication returns Option<usize> | Propagates NetworkPublication::new failure. Index used for publication_mut() |
| Cast u32 -> i32 for endpoint send_setup/heartbeat | Safe for valid term_length/mtu. Wire format uses i32. Follow-up to unify types |
| No term_buffer_length in DriverContext yet | add_publication takes term_length directly. Defer to step 4 |
| publication_mut() accessor | Simple, avoids wrapper. Callers (future DriverConductor) call offer() directly |

## Files to Modify

| Status | File | Description |
|--------|------|-------------|
| M | `src/agent/sender.rs` | PublicationEntry, add_publication, do_send, process_sm_and_nak |
| M | `tests/agent_duty_cycle.rs` | Update add_publication call, add offer+send test |

## Risks

| Risk | Mitigation |
|------|------------|
| Borrow checker rejects the destructure pattern | The pattern is well-known in Rust. If closure capture is the issue, use explicit local variables for closure captures |
| send_data inside sender_scan closure - lifetime of `data` | `data` is a slice into RawLog's buffer. Valid for the closure call. submit_send copies into send slot immediately |
| Performance: extra accessor calls vs direct field access | All accessors are `#[inline]`, trivial loads. No overhead after inlining |

