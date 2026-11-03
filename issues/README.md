# Issues

Each issue is a Markdown file named `NNN-short-title.md`, where `NNN` is its
number, zero padded to three digits. Open issues are in this directory, and
closed ones are in `closed/`.

- **Opening an issue:** use the next number, one more than the highest in
  this directory and `closed/`:

  ```sh
  ls issues issues/closed | grep -oE '^[0-9]{3}' | sort -n | tail -1
  ```

  Numbers are never reused, so a closed issue still counts.
- **The status line:** start each issue with a status line, such as
  `**Status:** open · **Size:** medium · **Related:** [003](003-....md)`.
- **Closing an issue:** set its status to `closed` and add a section saying
  what was done and how it differs from the plan. Then `git mv` it into
  `closed/`, and fix the relative links to it and from it.
