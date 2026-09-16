# Terminal interaction and performance

Builder's interactive frontend uses a crossterm event loop in the normal terminal buffer. It does not take over the alternate screen. Raw mode, bracketed paste, cursor visibility, and line wrapping are owned by a guard and restored on normal exit and error paths. The small composer adapts to resize events, including a compact layout for narrow or short terminals.

## Cost model

- **Paste:** one UTF-8 payload becomes one `Arc<str>` block plus a precomputed label. No character-by-character editor insertion or whole-payload repaint.
- **Typing around a paste:** only small display atoms change; pasted contents are not scanned.
- **Layout:** uses terminal cell widths and grapheme boundaries, and traverses display atoms rather than clipboard contents. Only a bounded set of rows is drawn.
- **Redraw:** queued input is drained for at most 8 ms before one synchronized frame. An isolated key is drawn immediately. There is no idle redraw loop.
- **Streaming:** output is coalesced every 16 ms or when the buffer reaches 8 KiB. Tool transitions, retries, and completion flush remaining output. Escape filtering maintains state across network chunks.
- **Undo/history:** paste snapshots share allocations. Drafts are limited to 4 MiB; history is capped at 100 entries / 16 MiB of logical payload. Undo has a 100-edit / 16 MiB budget. A limit rejection leaves the current draft unchanged.

These changes improve local interaction overhead. They cannot make a remote model generate faster or remove network latency.

## Reproduce the measurements

Observed locally on macOS with the optimized build:

| Measurement | Result |
| --- | --- |
| Warm process start → visible composer (median of 7 launches) | 11.0 ms |
| Exact 1 MiB bracketed paste → visible folded block | 85.6 ms |
| Typing after that paste → repaint | 0.1 ms |

The first launch immediately after building took 415.8 ms. Subsequent startup runs were 10.5–11.4 ms; the final full harness run was 13.3 ms. These are local observations, not promises for another terminal or machine. The paste measurement includes moving the bytes through the PTY, parsing, insertion, and repaint.

```sh
cargo build --release --locked
python3 tests/terminal_smoke.py target/release/builder
```

The Python harness uses a real Unix pseudo-terminal with bracketed-paste sequences and a local mock HTTP endpoint. It measures process startup to visible composer, a 1 MiB paste to visible acknowledgment, and typing immediately after the paste. It then presses Enter and compares the endpoint's request payload exactly, including CRLF, indentation, Unicode, and source comments beginning with `//`.

The same run checks that pasting did not auto-submit, output stayed compact, Tab completed a command, resizing to 44×18 and 20×5 didn't break input, and terminal paste mode was restored on exit. It uses only Python's standard library. Linux and macOS CI run this test; Windows runs the Rust model/layout tests but does not currently have an equivalent real-terminal harness.

Timings depend on the machine, terminal, build mode, and scheduler. They are diagnostic measurements, not hard timing assertions in CI. The harness fails on stalled input or incorrect behavior. Run it repeatedly when comparing a change; a single measurement is not a performance guarantee.


## Large transcript latency diagnosis

A 195,477-byte / 2,343-line transcript reproduced a local paste-to-block latency of 14.7 ms in the macOS PTY. A separate request containing that transcript took 58.36 seconds to receive response headers and its first reasoning delta from the configured hosted model. That separates terminal paste handling from endpoint latency; the measurement does not distinguish server queuing from prompt processing. It is not a guarantee for Apple Terminal's clipboard-to-PTY delivery.

Builder now labels the saved prompt awaiting the endpoint, connected-but-waiting output, model thinking, and tool-call preparation. Previously reasoning-only deltas left the generic waiting indicator unchanged. These labels expose activity; they do not reduce model inference time or trim the submitted transcript. The terminal harness simulates delayed headers and reasoning-only output and checks both progress labels before the final answer.

## Direct clipboard path on macOS

Ctrl+V invokes `/usr/bin/pbpaste` with bounded output and a two-second subprocess timeout, then feeds the result through the same atomic paste insertion. The composer shows a clipboard-reading status first. Terminal key events remain queued during the read, preserving insertion order. Errors leave the draft unchanged. UTF-8, whitespace, and line endings are retained; no content is automatically submitted. This bypasses terminal clipboard delivery; it does not change Cmd+V or reduce model latency.

To test the macOS clipboard shortcut against the local mock endpoint, run `python3 tests/terminal_smoke.py target/release/builder --clipboard`. This opt-in test reads the existing clipboard without replacing it and deletes its temporary history afterward. With text available, it checks exact payload preservation after Ctrl+V, Undo, and Redo; with an empty clipboard, it checks the error message and unchanged draft.


## Interrupt, steer, cancel, and rewind

During model output, Ctrl+C drops the generation future and restores the composer. Typing a follow-up changes direction without requiring `/retry`. `/retry` continues the committed turn; `/cancel` closes it without contacting the model. `/rewind` archives the last user turn and puts its prompt back in the composer, with no automatic submission. Rewound originals remain accessible with `/history archived`; workspace files are not rolled back.

`python3 tests/terminal_interrupt.py target/release/builder` uses a real PTY and local mock endpoint to test Ctrl+C during a stalled response, follow-up submission with earlier tool evidence, discarded partial output, editing a rewound prompt, cancellation without a new request, reopening paused sessions, and restoring a rewound draft across restart. It requires no credentials and never touches a real project.


## Automatic and manual compaction

`/status` shows the auto-compaction threshold (75% by default); the composer keeps only current context usage visible. The trigger includes estimated context, tool schemas, and reserved response tokens. `/compact` summarizes without continuing the task. Progress distinguishes summary generation from ordinary model output, and completion reports estimated before/after sizes. Original transcripts remain available in history. The real PTY test `python3 tests/terminal_compact.py target/release/builder` checks automatic triggering, output-limit and oversized-handoff retry and continuation, manual compaction, original history access, and checkpoint continuation after restart. A summary output-limit failure displays a compaction-specific retry notice; it gets one larger generation attempt per fragment while keeping the saved handoff small.

## Command discovery and draft navigation

Type `/` to open a filtered command menu. Up/Down browse the choices; Tab or Enter fills the selected command without running it. Enter on a complete command runs it. Escape closes the menu and keeps the text. The menu shares the bounded composer area and shows the selected command even in short terminals.

Arrow keys move through draft lines without replacing a new draft at its boundaries. Up on an empty composer opens history; Down returns to newer entries and restores the unsent draft. Ctrl+P/N explicitly browse history from a populated draft. The hint shows the history position. Session status puts paused-turn guidance and approval mode before context details.

## Model stalls and empty responses

Model progress shows total elapsed time for the current attempt. Streamed
reasoning changes the label to `Thinking`. A separate live signal reads
`receiving now` while response data is arriving, then changes to
`no data for …` as silence grows. Incoming fragments do not reset the total
clock. The profile's idle and total request timeouts remain the hard bounds, and
the configured generation budget is unchanged.

A completed response with no answer or tool call leaves the turn pending. Builder
reports reasoning-only responses separately from empty responses, and does not
repeat an expensive generation automatically. Whitespace alone is not an answer.
Reasoning text is never substituted for a final answer or persisted as one.

For a llama.cpp thinking model that spends its budget without producing usable
output, disable thinking in that profile's existing `extra_body` table:

```toml
[profiles."your llama.cpp profile".extra_body]
chat_template_kwargs = { enable_thinking = false }
```

This is a llama.cpp chat-template option, not a universal provider setting. See
[the server API](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md).
Restart Builder after changing configuration, resume the session, then use
`/retry`. If responses remain empty, inspect the server's model/chat-template
configuration; repeatedly increasing the output limit may only lengthen the wait.

### Pipeline settings

Choose **Settings** from the `/` menu to open the pipeline panel. Up/Down browse,
Enter/Space toggle switches, Enter edits a numeric limit, and End selects Save.
Escape cancels (or leaves a numeric edit). Restore defaults is staged until Save.
Saved settings take effect in the current session and its profile immediately;
opening the panel does not call the model or resume pending work. Plain mode offers
a numbered menu. The rich panel temporarily uses an alternate screen, restoring
the inline composer and existing scrollback afterward.

Validate keyboard interaction, cancellation, persistence and live policy with:
`python3 tests/terminal_pipeline.py` after `cargo build --locked`.

## Visual hierarchy

The composer uses open rules, a 100-column maximum width, and one to four draft
rows. It grows with the message; command choices share the same eight-row budget.
Keyboard movement uses the exact rendered content width, including short and
narrow terminal fallbacks. Shortcuts change with the draft, history, or menu.

Lavender marks focus, neutral rules separate content, and filled rows identify
selected commands and settings. Paused guidance uses the warning color and stays
first in the status line. Ordinary short messages omit byte counters. Model and
compaction details are available through `/status`. Tool progress occupies a
transient indicator, then resolves into one result row with timing.
Submitted prompts and streamed replies wrap at word boundaries inside a
96-column reading measure. Tool rows omit meaningless `0.0s` timings, keep long
commands bounded, and wrap failure details beneath the affected action.

`NO_COLOR`, plain mode, normal scrollback, and existing approval and recovery
behavior remain supported.
