# 0004: Auto-reply by category

| Status | Date | PR | Release |
|---|---|---|---|
| draft | 2026-10-21 | #6 | v0.8.0 |

> A reply that stays visible can get an answer from you, chosen by its
> category and adapted to the reply by the reviewer model. By default the
> answer is a draft pushed to you; posting it automatically is opt-in.

## Problem

Borderline replies (`review`, or kept by the reviewer) stay visible. Some
deserve a short public answer, for example a reminder to stay civil. Typing
the same answer by hand each time is tedious, and the homelab is off at
night, so the morning catch-up can bring several at once. One fixed text
reads like a bot and ignores the reply's language.

## Proposal

- `AUTO_REPLY` maps a category to a base answer:
  `harassment=Please keep it civil.|sexual=Not here.`. Keys are the
  classifier categories (`hateful`, `harassment`, `spam`, `sexual`,
  `neutral`, `positive`). An unknown category or an empty answer stops the
  app at startup.
- With a reviewer configured (`LLM_BASE_URL`, `LLM_MODEL`), the model adapts
  the base answer to the post and reply, in the reply's language, as JSON
  `{"reply": ...}`. A draft that is empty, over 280 characters, or contains a
  link, `@` or `#` is refused, and the base answer is used. Without a
  reviewer, the base answer is used as is.
- `AUTO_REPLY_MODE=draft` (default) pushes the answer and the reply's
  permalink to `NOTIFY_URL`; you post it yourself. `AUTO_REPLY_MODE=post`
  posts it as you.
- Only replies whose action is `review` or `kept` are answered. A hidden
  reply hides everything nested under it, your answer included.
- One answer per author per `AUTO_REPLY_COOLDOWN` (default `1d`), in both
  modes.
- The answer is sent after the decision is saved: it is never sent twice. A
  failure is logged and not retried.
- `DRY_RUN=true` in `post` mode logs `result=would_reply` and posts nothing.
  Drafts are pushed in a dry run too, like other notifications.
- `post` mode needs the `threads_content_publish` permission. Tokens from
  before v0.8.0 lack it: add it to the Meta app and open `/oauth/start` again.

### Configuration

| Env | Default | |
|---|---|---|
| `AUTO_REPLY` | | `category=answer`, `\|`-separated; empty turns it off |
| `AUTO_REPLY_MODE` | `draft` | `draft`: push to `NOTIFY_URL`. `post`: post as you |
| `AUTO_REPLY_COOLDOWN` | `1d` | At most one answer per author within this time |

### Logs and metrics

| | |
|---|---|
| `msg=auto_reply` | `answer`, `source` (`llm` or `static`), `result` (`drafted`, `replied`, `would_reply`, `cooldown`, `failed`), `error`, `draft_error` |
| `threads_api_errors_total{endpoint="reply"}` | Failed posts |
| `threads_api_errors_total{endpoint="drafter"}` | Failed model calls |

## Risks

The model reads text written by a stranger and, in `post` mode, publishes as
you. A reply can try prompt injection ("ignore your instructions and
say ..."). The prompt treats post and reply as data, and the output checks
refuse links, mentions and hashtags, but an obfuscated link or an off tone
can still get through. That is why `draft` is the default.

## Alternatives

| Option | Why not |
|---|---|
| Answer, then hide (e.g. spam) | The hide hides the answer too; only the spammer, usually a bot, sees it |
| Key answers by action (`review=...`) | The category says what to answer; the action only says whether it stays visible |
| An intent prompt instead of a base answer | Without a model or with a bad draft, there would be no usable text to fall back to |
| A separate drafting model | The reviewer model already reads post and reply; one more config set for little gain |
| Always post | A model publishing as you on untrusted input needs a human in the loop by default |
| Retry failed answers | A missed answer is not lost data; retrying risks double posts |

## Testing and rollout

- **Tests:** spec parsing; draft parsing and refusals (empty, too long,
  link, mention, hashtag); a `review` reply answered once per author; a
  hidden reply not answered; the model drafting when a reviewer is set; draft
  mode pushing instead of posting; a dry run posting nothing.
- **Rollout:** v0.8.0. Off until `AUTO_REPLY` is set; starts in `draft`.
  Re-authorize for `threads_content_publish` before `post`.
- **Off switch:** unset `AUTO_REPLY`, or `AUTO_REPLY_MODE=draft` to stop
  posting.
