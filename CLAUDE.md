Always use Context7 MCP when I need library/API documentation, code generation, setup or configuration steps without me
having to explicitly ask.

Always use rust-analyzer as well as `cargo check` before running code.

Commit your changes to git with git semantic commit messages in the format of <type>(<scope>): <subject>

Where <type> is one of the following:

- feat: A new feature
- fix: A bug fix
- docs: Documentation only changes
- style: Changes that do not affect the meaning of the code (white-space, formatting, missing semi-colons, etc)
- refactor: A code change that neither fixes a bug nor adds a feature
- perf: A code change that improves performance
- test: Adding missing tests or correcting existing tests
- chore: Changes to the build process or auxiliary tools and libraries such as documentation generation

Where <scope> is the section of the codebase affected (e.g., module or component name).

Where <subject> is a brief description of the change in imperative mood (e.g., "add feature", "fix bug").

Don't provide a further description to the commit if not strictly necessary.
