# 0004: Auto-reply by category

| Status | Date | PR | Release |
|---|---|---|---|
| draft | 2026-10-21 | #6 | v0.8.0 |

> A reply that stays visible can get an answer from you, chosen by its
> category, such as "Please keep it civil." for borderline harassment.

## Problem

Borderline replies (`review`, or kept by the reviewer) stay visible. Some
deserve a short public answer, for example a reminder to stay civil. Typing
the same answer by hand each time is tedious, and the homelab is off at
night, so the morning catch-up can bring several at once.

## Proposal

- `AUTO_REPLY` maps a category to an answer:
  `harassment=Please keep it civil.|sexual=Not here.`. Keys are the
  classifier categories (`hateful`, `harassment`, `spam`, `sexual`,
  `neutral`, `positive`). An unknown category or an empty answer stops the
  app at startup.
- Only replies whose action is `review` or `kept` are answered. A hidden
  reply hides everything nested under it, your answer included, so answering
  it is pointless.
- One answer per author per `AUTO_REPLY_COOLDOWN` (default `1d`), so a
  persistent author does not get the same line ten times.
- The answer is sent after the decision is saved: it is never sent twice. A
  failure is logged and not retried.
- `DRY_RUN=true` logs `result=would_reply` and sends nothing.
- Needs the `threads_content_publish` permission. Tokens from before v0.8.0
  lack it: add it to the Meta app and open `/oauth/start` again.

### Configuration

| Env | Default | |
|---|---|---|
| `AUTO_REPLY` | | `category=answer`, `\|`-separated; empty turns it off |
| `AUTO_REPLY_COOLDOWN` | `1d` | At most one answer per author within this time |

### Logs and metrics

| | |
|---|---|
| `msg=auto_reply`, `result` | `replied`, `would_reply`, `cooldown` or `failed` (with `error`) |
| `threads_api_errors_total{endpoint="reply"}` | Failed answers |

## Alternatives

| Option | Why not |
|---|---|
| Answer, then hide (e.g. spam) | The hide hides the answer too; only the spammer, usually a bot, sees it |
| Key answers by action (`review=...`) | The category says what to answer; the action only says whether it stays visible |
| `,` or `;` as separator | Both are common in answer text; `\|` is not |
| Retry failed answers | A missed answer is not lost data; retrying risks double posts |

## Testing and rollout

- **Tests:** spec parsing (valid, unknown category, empty answer); a `review`
  reply answered once per author; a hidden reply not answered; a dry run
  sending nothing.
- **Rollout:** v0.8.0. Off until `AUTO_REPLY` is set. Re-authorize for
  `threads_content_publish` first.
- **Off switch:** unset `AUTO_REPLY`.
