# netfyr

## Commit messages

```
Subject line in imperative mood (Add/Fix/Implement/Use/Update ...)

Body: 1-3 sentences explaining what changed and why.
Wrap lines at ~72 characters.

Story: NNN-spec-slug

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>
```

- Subject starts with a capitalized verb in imperative mood, no trailing period
- Blank line between subject and body
- `Story:` trailer references the spec number and slug (e.g. `352-history-cli`)
- `Co-Authored-By:` trailer is always last
- When fixing a bug introduced by a previous commit, add a
  `Fixes: HASH12 ("COMMIT SUBJECT")` trailer (12-digit hash, subject in
  parentheses and quotes) before the `Co-Authored-By:` line

## Bug fix workflow

1. Write a test that reproduces the bug. The test must exercise the
   existing buggy code path and fail because the bug produces wrong
   results — not because new fix code is missing. A compilation error
   is never a valid reproduction. When the bug is reported as a series
   of steps (e.g. "do X, then Y, then Z fails"), write an integration
   test (see below) that follows those steps. If the bug involves
   multiple components (daemon, DHCP, CLI, reconciler), also use an
   integration test. Reserve unit tests for new feature development,
   where you need to verify that a single function behaves as expected.
2. Run the test and verify it fails **for the right reason** (the bug)
3. Write the fix
4. Run the test again and verify it passes
5. Run `cargo test` and `make integration-test` to verify no regressions
6. Check if the bug was introduced by a previous commit; if so, add a
   `Fixes:` trailer to the commit message (see above)

## Integration tests

Tests that require the daemon or multiple components (e.g. the CLI)
must be written as shell scripts in `tests/`. They run inside an
unprivileged user+network namespace via `netns_setup`. Look at existing
tests for reference.

## Documentation

After each change, check if relevant documentation (man pages,
README.md, etc.) needs updating.
