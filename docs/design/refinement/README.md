# Interface refinement · September 10, 2026

The browser uses a neutral visual direction. The terminal adds restrained color:
cyan for the brand, prompt, and response headings; soft blue for paths and
keyboard shortcuts; amber for pasted blocks; green for saved or successful
states; and peach for warnings. Selections use pale cyan on charcoal.
Typography, alignment, and spacing establish hierarchy. The terminal keeps the
user's background and ordinary text foreground, and respects `NO_COLOR`.
No new runtime dependencies were introduced.

## Research and application

- [Linear's 2026 interface design notes](https://linear.app/now/behind-the-latest-design-refresh):
  navigation should recede, and controls should appear in predictable places.
  The browser uses a subdued sidebar, a persistent conversation title, and a
  single menu for occasional conversation actions. The composer stays visible
  on desktop, with folder, profile, and permissions grouped together.
- [NN/g: Aesthetic and minimalist design](https://www.nngroup.com/articles/aesthetic-minimalist-design/):
  remove irrelevant information without removing necessary controls. The empty
  state provides two editable starting prompts. Promotional headings, decorative
  sparkles, and redundant labels have been removed. Rewound-history controls only
  appear for saved conversations.
- [Command Line Interface Guidelines](https://clig.dev/):
  familiar conventions, discoverable commands, and sufficient progress feedback
  make a terminal usable. The terminal keeps inline scrollback, explicit state
  labels, command selection markers, timing, and cancellation hints. Progress
  wording is shorter and bounded to the terminal width. A labeled composer and
  consistent selection styling carry through the command and settings interfaces.

These are design decisions informed by the sources, not claims of user-study
results or equivalence to those products.

## Review captures

- [Browser workspace](browser.png)
- [Phone conversation](mobile.png)
- [Terminal composer](terminal.png)
- [Terminal command menu](commands.png)
- [Narrow terminal](narrow.png)

Browser captures use local fixture conversations and profiles, not a live model
or user data. Terminal captures render actual PTY output with Menlo on a sample
dark background. The original images in the parent directory document the
previous design.

## Validation

The real browser was checked at 320, 390, 560, 561, 768, 1024, and 1440 pixels:
no page overflow or JavaScript errors. Checks also covered 200% enlargement,
suggestion-to-composer focus, the phone drawer's Tab/Shift+Tab loop, Escape,
restored focus, and changing from phone to desktop with the drawer open.
An axe scan of the connected workspace reported no WCAG A/AA violations;
automated checks do not replace assistive-technology or user testing.

The existing browser uncertainty test still verifies that failed submissions
preserve drafts and are never replayed. Real PTY tests cover a 1 MiB paste,
exact payload preservation, command completion, narrow resizing, terminal
cleanup, and settings persistence/cancellation. Formatting and strict Clippy passed.
The workspace suite passed with 205 tests and 6 opt-in tests ignored, using
`TMPDIR=/private/tmp cargo test --workspace --locked`. macOS’s default aliased
temporary path causes the existing CLI-subfolder fixture to mismatch the
canonical workspace path; using a canonical temporary root avoids that fixture
issue without changing workspace boundary behavior.
