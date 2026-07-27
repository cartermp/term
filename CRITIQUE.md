# Codebase Critique: `term`

This is a high-quality personal project with several advanced features (GPU rendering via `wgpu`, ligature shaping via `rustybuzz`, and a clean event-driven architecture). However, looking through an "extremely critical lens," there are several architectural and implementation bottlenecks that prevent it from being a truly "extremely well-done" terminal emulator.

## 1. Grid Storage & Scrolling Performance
The current implementation of the terminal grid is a `Vec<Vec<Cell>>`.

**Status:** Partially addressed. The hot scrolling/line-edit paths no longer use `Vec::remove`/`Vec::insert` churn in the common case; they now rotate/reuse existing row buffers in place. The grid is still `Vec<Vec<Cell>>`, so a fully flattened circular buffer remains future work if profiling shows the remaining structure is still a bottleneck.

*   **Critique:** This causes $N$ heap allocations for a grid of $N$ rows. More importantly, scrolling up involves `grid.remove(0)` and `grid.insert(idx, blank)`. These are $O(rows)$ operations that involve shifting every `Vec` pointer in the outer `Vec` and frequent reallocations.
*   **Recommendation:** Flatten the grid into a single `Vec<Cell>` and use a circular buffer (tracking a `start_row` index) to make scrolling an $O(1)$ operation.

## 2. Unicode & CJK Support
The terminal currently assumes that every character occupies exactly one column.

**Status:** Mostly addressed. The terminal now uses `unicode-width` to treat wide characters as two-column cells with explicit continuation placeholders, and erase/overwrite paths were updated so those placeholders stay coherent. The hand-maintained grapheme-extension list is still present, so a future move to a fuller grapheme-segmentation model remains worthwhile.

*   **Critique:** East Asian (CJK) characters are "wide" and should occupy two columns. Printing a wide character currently results in it overlapping the next character, or breaking alignment for all subsequent text on that line. The `is_grapheme_extend` list is also a manual maintenance burden.
*   **Recommendation:** Integrate the `unicode-width` crate to correctly handle character widths and the `unicode-segmentation` crate for robust grapheme cluster detection. Use a "placeholder" cell (e.g., a special `Cell` type or flag) to represent the second half of a wide character.

## 3. Rendering Pipeline Efficiency
Each frame, the renderer performs text shaping for every visible row via `rustybuzz`.

**Status:** Addressed. The renderer caches per-row glyph operations and also caches the complete static frame. Cursor blink and ghost-text changes are emitted separately, so an idle redraw neither scans the grid nor re-runs HarfBuzz.

*   **Critique:** Shaping is relatively expensive. In a 100-row terminal, we are shaping ~20,000 cells every frame (~60-120 times per second), even if the content is static.
*   **Recommendation:** Implement a dirty-row mechanism or a shaping cache. Only re-shape rows that have changed since the last frame.

## 4. PTY Processing & Main Thread Blocking
PTY data processing happens on the main thread inside the `winit` event loop.

**Status:** Partially addressed. VT mutation remains on the main thread to keep the grid single-owned, but PTY reads now feed a bounded 256 KiB staging buffer with backpressure and one coalesced wake event. This removes the unbounded event/allocation failure mode and bounds each main-thread drain. A background parser would still require an immutable snapshot or damage-list handoff.

*   **Critique:** If a command produces a massive burst of output (e.g., `cat`ing a large file), the main thread will spend significant time in `Terminal::process`, causing the UI to stutter or drop frames.
*   **Recommendation:** Move the VT state machine (`vte` processing) to a background thread. The main thread should only consume "snapshots" of the grid or a list of dirty regions for rendering.

## 5. Atlas Management
The glyph atlas is fully cleared when it overflows.

**Status:** Addressed for the current font model. The atlas is now a four-layer texture array. It advances to a fresh stable layer instead of clearing entries whose UVs may still be referenced by cached frames. At the fixed bundled font size this provides 4 MiB of glyph coverage without eviction stalls; once exhausted, previously cached glyphs remain valid.

*   **Critique:** While rare for standard English text, a terminal displaying many unique Unicode characters (or different font sizes if that were added) would frequently trigger a full clear, leading to visible stutters as every glyph is re-uploaded.
*   **Recommendation:** Implement an LRU (Least Recently Used) eviction policy for the atlas, or at least a multi-stage atlas that grows/compacts more gracefully.

## 6. PTY Data Passing
Data from the PTY reader thread is sent to the main thread via `Vec<u8>` for every `read()` call.

**Status:** Addressed. Reader threads append into a reusable, bounded per-pane buffer. Multiple reads coalesce behind a single `PtyReady` notification, and the main thread swaps the bytes into a reusable scratch vector. Child exit is reported only after PTY EOF, preserving final output ordering.

*   **Critique:** This creates high allocation pressure (thousands of small `Vec`s per second during high output).
*   **Recommendation:** Use a lock-free ring buffer or a pool of pre-allocated buffers to pass data between the PTY thread and the processing thread.

---

# Next Steps

1.  **Refactor Grid Storage:**
    *   Flatten `Vec<Vec<Cell>>` into `Vec<Cell>`.
    *   Implement circular buffer indexing for $O(1)$ scrolling.
2.  **Robust Unicode Support:**
    *   Add `unicode-width` and `unicode-segmentation`.
    *   Handle double-width characters in `put_char` and `visual_cell`.
3.  **Profile Before Further Renderer Work:**
    *   Measure sustained-output frame time and GPU upload volume.
    *   Add row-level GPU buffer patching only if static-frame invalidation remains material.
4.  **Consider Background VT Parsing:**
    *   Prototype immutable grid snapshots or compact damage lists.
    *   Keep terminal state single-owned; do not add shared mutable grid state.
