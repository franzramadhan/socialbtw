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

## Deploy every release to the homelab

A release is done only when it runs in the homelab and its new features are
checked there. The homelab repo is
`~/works/personal/lab.franzramadhan.dev/infrastructure`
(`franirafa/infrastructure`); follow its `CLAUDE.md` too.

1. **Branch** `feat/socialbtw-X.Y.Z` from an up-to-date `main`. If the
   checkout is on another branch, use a `git worktree` instead of switching.
   One socialbtw PR at a time: if an older one is still open, merge it first
   or fold the bump into it.
2. **Manifest** `kubernetes/apps/socialbtw/deployment.yaml`: pin
   `ghcr.io/franzramadhan/socialbtw:X.Y.Z@sha256:<digest>` (digest from
   `docker buildx imagetools inspect`) and add the env for new features.
   New features start in their safest mode (`AUTO_REPLY_MODE=draft`, a narrow
   `HIDE_PATTERN`, ...). New secrets go through `externalsecret.yaml`.
3. **Docs** in the same PR: `kubernetes/apps/socialbtw/README.md` and the
   tables in `kubernetes/README.md`.
4. **PR** titled `feat(kubernetes): socialbtw X.Y.Z, <feature>`. When CI is
   green, self-merge (`gh pr merge --squash --admin`). Never between 23:30 and
   06:00 WIB: the cluster is off.
5. **Roll out:** `flux reconcile kustomization socialbtw -n flux-system --with-source`.
6. **Test the new features there:** the pod runs X.Y.Z, startup logs show no
   config error, the next poll succeeds
   (`threads_last_poll_success_timestamp_seconds`), and the feature's own log
   line or metric shows up (`stage=pattern`, `msg=auto_reply`, ...). If it
   needs a reply to trigger, post one from a test account. Report what was
   seen.
7. **Fix forward:** a broken feature gets a socialbtw patch release or a new
   homelab PR, never a manual `kubectl` change. Switching to a riskier mode
   (`DRY_RUN=false`, `AUTO_REPLY_MODE=post`) is a separate PR, after the user
   agrees.
