Review the commits made to this repository in the last two weeks (`git log --since="2 weeks ago" -p`).

Look for real defects: logic errors, unhandled errors, race conditions, resource leaks, security problems and broken edge cases. Fix only the ones you are confident about and can verify, and add a regression test for each fix where the project has tests.

Don't restyle code, rename things or refactor for taste. If you find nothing worth fixing, change nothing and report `no_change`.
