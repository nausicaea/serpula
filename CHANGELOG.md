## v0.1.0 (2026-10-17)

### Feat

- remove the success notification again because it was leading to too many notifications
- add JSON output for restic
- add a notification on success
- add PATH to the environment variables during installation
- remove the command line unit tests that rely heavily on mocking
- have Claude fix some bugs and add additional unit tests
- nail down permissions of the secret file
- add verbose output to the unit tests
- excise all rust code
- excise the rust-related Github Actions and Pre-commit hooks
- add a no-dependency Python implementation in a single file
- add additional exclusions to the backup
- add priority handling for ntfy notifications
- generalize command line arguments to accept any subcommands supported by radicle
- change the project identifier
- replace the machine-rolled logger with the log crate, and also replaced the app error with anyhow
- begin cleaning up the claude-generated code
- improve coverage by claude
- modularize the code by claude
- commit by claude
- add a Cargo.lock file
- initial commit

### Fix

- anonymize the notification message
- anonymize the notification message
- replaced nargs='*' with action='append'
- log_dir pointed to the wrong directory on macOS
- pin Python version to 3.14.7
- replace pre-commit hook run with unittesting only
- resolve some leftover type annotation issues
- resolve an unfixable mypy type warning
- resolve an issue with mixing ABCs and dataclasses
- pin dependencies
- add a missing argument to the test for notify
- oof. loads of fixes
- return the trimmed ntfy.sh topic from NTFY_TOPIC environment variable retrieval

### Refactor

- downgrade Python to 3.9
- once tested, rely on the constant RDN in unit tests
- explicitly convert the tilde to the expanded home directory, and add a default hostname tag
- convert generator comprehension to dict comprehension
- restructure the plist file generation for better maintainability
- rename the sanitize() function and have Claude add tests and implementation that follow RFC 1035.
- move the binary-specific code into main.rs
- individual modules
