Find code changed in the last two weeks (`git log --since="2 weeks ago" --stat`) that has no tests or weak ones, and add focused tests for its important behaviour and edge cases.

Follow the project's existing test style, framework and layout. Every test you add must pass. If a test exposes a real bug, fix the bug only if the fix is small and obvious; otherwise leave the test out and describe the bug in your summary.

If coverage of the recent changes is already good, change nothing and report `no_change`.
