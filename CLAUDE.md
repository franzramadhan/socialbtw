# CLAUDE.md

socialbtw: hides unwanted replies on your own Threads posts. Rust, one binary.
See README.md for what it does and src/ for the code map.

## Workflow for every feature or change

1. **Gather the requirements.** Ask what is unclear before writing code: the
   goal, who it is for, limits, and what "done" looks like.
2. **Draft a short RFC** in `rfc/`, copied from `rfc/0000-template.md`. Name it
   `NNNN-short-title.md`, with `NNNN` the next free 4-digit number, so files
   sort by creation in git. Keep it short; update it if the plan changes.
3. **Commit with [Conventional Commits](https://www.conventionalcommits.org):**
   `type(scope): summary` in the imperative, lowercase, no period. Types:
   `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`.
   Breaking changes: `!` after the type and a `BREAKING CHANGE:` footer.
   Commits are GPG-signed.
4. **Open the PR as a draft** (`gh pr create --draft`) from a branch named
   `<type>/<short-title>`. The PR title is a Conventional Commit too: it
   becomes the squash commit. Link the RFC in the body.
5. **When CI passes, mark it ready** (`gh pr ready`). If CI fails, fix and push
   again; the PR stays a draft until it is green.
6. **Merge with squash** (`gh pr merge --squash --delete-branch`).

Before pushing: `cargo test --locked` passes locally (CI runs the same on
every PR). **Releases:** bump the version in `Cargo.toml` in the feature PR. After the
merge, on `main`: `git tag -s vX.Y.Z -m vX.Y.Z && git push origin vX.Y.Z`
(`image.yml` publishes `ghcr.io/franzramadhan/socialbtw:X.Y.Z`), then
`gh release create vX.Y.Z --verify-tag` with short notes: what changed, new
config, breaking changes, and the RFC.
