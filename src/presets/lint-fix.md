Run the linters, formatters and type checkers this project already uses (look at its CI config, package scripts, Makefile, Cargo/Go/Python tooling) and fix the warnings and errors they report.

Don't add new tools, new lint rules or new dependencies, and don't silence warnings with ignore comments unless the warning is a false positive (say so in the summary). Keep each change minimal and behaviour-preserving, and run the tests afterwards.

If everything is already clean, change nothing and report `no_change`.
