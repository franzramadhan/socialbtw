# 0001: Rate-limit handling and read budget

- Status: implemented (v0.5.0, written after the fact)
- Date: 2026-10-19
- PR: #2

## Problem

The Threads API rate limits read calls. A rate-limited call only failed the
poll, and the next poll retried after the usual 5 minutes, without backing off.
A failed hide also dropped the decision, so the reply was classified (and paid
for) again on every retry. Accounts that post a lot read every due post in
every poll, in pages of 25.

## Proposal

- Treat HTTP 429 and Meta error codes 4, 17, 32 and 613 as rate limits.
- A rate limit stops the poll and pauses polling: `RATE_LIMIT_BACKOFF` (15m)
  first, doubling while it continues, up to `RATE_LIMIT_MAX_BACKOFF` (6h). A
  good poll resets it. Metric `threads_rate_limited_until_timestamp_seconds`.
- A failed hide is stored as `hide_pending` and retried at the start of each
  poll without classifying again. A 4xx (the reply is gone) ends as
  `hide_failed`.
- `MAX_POSTS_PER_POLL` (50) caps posts read per poll, most overdue first;
  the rest count as `over_budget`. Lists use 100 items per page.

## Alternatives

- Reading Meta's usage headers to slow down before a limit: not documented for
  Threads, so the app reacts to errors instead.
- A fixed retry delay: either too slow to recover or too aggressive under a
  long limit; doubling covers both.

## Testing and rollout

Unit tests for detection, backoff, due-post ordering with the cap, a
rate-limited hide that ends `hidden` after one retry without a second
classification, and a reply that is gone. Ship as v0.5.0; the defaults need no
config. To turn the cap off: `MAX_POSTS_PER_POLL=0`.
