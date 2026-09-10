# Terminal design

The [September 10 refinement](refinement/README.md) documents the current browser
and terminal direction, research, and review captures. The notes and images
below are the September 8 baseline.

Builder uses a quiet, inline workspace: one lavender accent, neutral rules,
ordinary terminal foreground for content, and a consistent left alignment.
The terminal supplies the background and font. No alternate screen is used for
conversation, so normal selection and scrollback remain available.

## Research and decisions

Reviewed September 8, 2026:

- [Claude Code interactive mode](https://code.claude.com/docs/en/interactive-mode):
  keyboard discovery, command completion, and contextual controls. Builder shows
  only the shortcuts relevant to the draft or command selection; `/help` retains
  the full reference.
- [Claude Code status lines](https://code.claude.com/docs/en/statusline): separate
  session information from the conversation. Builder keeps approval state and
  context usage visible, with detailed configuration in `/status`.
- [Charm Crush](https://github.com/charmbracelet/crush): inspiration for a distinct
  terminal identity. Builder uses a restrained accent and a filled selection row
  shared by commands and settings.
- [Codex CLI](https://github.com/openai/codex): an additional reference for the
  terminal coding-agent workflow. Builder keeps its existing inline architecture.

These are design interpretations, not claims of feature equivalence.

## Interaction hierarchy

The startup header identifies the workspace, session, profile, and approval mode
in three lines. Home-relative paths reduce visual noise. The composer grows from
one to four text rows within an eight-row budget, caps its width at 100 columns,
and uses open horizontal rules instead of a labelled box. Draft sizes appear
only for large messages or folded pastes; a scrolled draft shows its line range.

Command selection has a filled row and an explicit marker, so it is also legible
without color. Selection and execution remain separate. Paused-turn guidance is
highlighted and appears before secondary status details.

Each response has one heading, streamed text shares the transcript margin, and
tools use a transient activity indicator followed by one durable result row.
Result rows retain failure markers and show elapsed time. The turn footer shows
elapsed time and action count rather than network byte counts.

## Visual review

The PNGs in this directory are rendered from actual CLI pseudo-terminal output
with an isolated mock profile. They illustrate a dark terminal with Menlo;
the application does not force that background or font. They are not browser
mockups. Ready, command, multiline, settings, and narrow layouts were inspected.
Live model quality and endpoint speed are outside this visual check.
