# socialbtw

Hides unwanted replies on your own [Threads](https://www.threads.com) posts.

Every few minutes it reads new top-level replies on your recent posts and
classifies each one with [Cloudflare Clef](https://developers.cloudflare.com/workers-ai/models/clef/)
as `hateful`, `harassment`, `spam`, `sexual`, `neutral` or `positive`. Clear
cases are hidden through the official Threads API. Uncertain cases can go to
any OpenAI-compatible LLM for a second opinion, or be flagged for you to
review. Every decision is a JSON log line and a Prometheus metric, so you can
alert on it from Grafana or anything else.

Works with Indonesian, English and mixed replies (including judi online and
slot spam).

## What it does not do

- **Block users or like replies.** The Threads API has no endpoint for either.
- **Delete replies.** Hiding is the strongest action the API allows.
- **Nested replies.** Only top-level replies can be hidden; hiding one also
  hides everything under it.
- **Image-only replies.** Replies without text are skipped.

## How a reply is decided

```
new reply ──> Clef: probability per category
                │
   P(bad) >= HIDE_THRESHOLD ──────────────> hide
   P(bad) >= REVIEW_THRESHOLD ──> LLM set? ── yes ──> hide / keep / review
                │                    └─ no ────────> review
   otherwise ─────────────────────────────> keep
```

`P(bad)` is the sum of the four bad categories. Each reply is decided once and
remembered in SQLite. A reply you unhide by hand is never hidden again.

**Start in dry-run** (the default). Decisions are logged as `would_hide` and
nothing changes on Threads. Look at the `threads_clef_bad_probability`
histogram and the logs for a few days, tune the thresholds, then set
`DRY_RUN=false`. Replies seen in dry-run are not revisited afterwards.

## Setup

### 1. Threads access token

1. Create an app at <https://developers.facebook.com/apps> with the
   **Access the Threads API** use case.
2. Add permissions `threads_basic`, `threads_read_replies` and
   `threads_manage_replies`.
3. Add your own Threads account as a tester (App roles), accept the invite in
   Threads (Settings, Account, Website permissions).
4. Generate a token (for example with the Graph API Explorer on
   `graph.threads.net`) and exchange it for a long-lived one (60 days):
   ```bash
   curl "https://graph.threads.net/access_token?grant_type=th_exchange_token&client_secret=$APP_SECRET&access_token=$SHORT_TOKEN"
   ```

The app stays in development mode; no app review is needed for your own
account. The token is refreshed every 24 hours and stored in the database, so
it does not expire while the app runs. Setting a different
`THREADS_ACCESS_TOKEN` replaces the stored one.

### 2. Clef

Create a Cloudflare API token with **Workers AI: Read** and note your account
ID. Clef's free tier (10,000 Neurons/day) covers a typical personal account.

Clef uses Cloudflare's own request format, not the OpenAI one. Behind
[LiteLLM](https://docs.litellm.ai), add a pass-through endpoint to
`https://api.cloudflare.com/client/v4/accounts/<id>/ai/run` and point
`CLEF_URL` at it instead.

### 3. Run

```bash
docker run -d --name socialbtw \
  -e THREADS_ACCESS_TOKEN=... \
  -e CLEF_URL=https://api.cloudflare.com/client/v4/accounts/<account-id>/ai/run/@cf/cloudflare/clef \
  -e CLEF_TOKEN=... \
  -v socialbtw:/data -p 9090:9090 \
  ghcr.io/franzramadhan/socialbtw:main
```

Without Docker: `pip install -r requirements.txt && python threads_monitor.py`.

## Configuration

| Env | Default | |
|-----|---------|---|
| `THREADS_ACCESS_TOKEN` | required | Long-lived Threads token |
| `CLEF_URL` | required | Clef endpoint (Cloudflare or a pass-through proxy) |
| `CLEF_TOKEN` | required | Bearer token for `CLEF_URL` |
| `CLEF_MODEL` | `clef` | `clef` (27B, more accurate) or `clef-flash` (9B, cheaper) |
| `LLM_BASE_URL` | empty | OpenAI-compatible base URL, e.g. `http://litellm:4000/v1`. Empty: uncertain replies go to `review` |
| `LLM_API_KEY` | empty | API key for the LLM |
| `LLM_MODEL` | empty | Model name; a small cheap one is enough |
| `EXTRA_RULES` | empty | Your own policy, appended to the LLM prompt (e.g. `Mild swearing between friends is fine.`) |
| `DRY_RUN` | `true` | `false` to actually hide replies |
| `HIDE_THRESHOLD` | `0.85` | P(bad) at or above this hides |
| `REVIEW_THRESHOLD` | `0.5` | P(bad) at or above this escalates |
| `TAGS` | empty | Comma-separated tags; only posts whose topic tag or a `#hashtag` in the text matches are monitored (case-insensitive, `#` optional). Empty: all posts |
| `ALLOWLIST` | empty | Comma-separated usernames never touched |
| `POLL_INTERVAL_SECONDS` | `300` | |
| `LOOKBACK_DAYS` | `3` | How far back to read your posts. Each poll costs 1 call per post; keep it small if you post a lot |
| `DB_PATH` | `/data/threads-monitor.db` in the image | SQLite file. Holds the refreshed token: keep it private |
| `METRICS_PORT` | `9090` | Prometheus `/metrics` |

## Observability

Logs are one JSON object per line on stdout. Each decision has `msg=decision`
plus `reply_id`, `username`, `text`, `category`, `bad_probability`,
`probabilities`, `action` (`hidden`, `would_hide`, `kept`, `review`), `stage`
(`clef` or `llm`), `reason` and `permalink`.

| Metric | |
|--------|---|
| `threads_replies_total{category,action,stage}` | Decisions |
| `threads_clef_bad_probability` | Histogram of P(bad), for tuning thresholds |
| `threads_api_errors_total{endpoint}` | Failed calls: `posts`, `replies`, `hide`, `refresh`, `clef`, `llm` |
| `threads_last_poll_success_timestamp_seconds` | Last good poll |
| `threads_token_expiry_timestamp_seconds` | Token expiry, 0 until the first refresh |

Suggested alerts:

```promql
# replies waiting for you
increase(threads_replies_total{action="review"}[15m]) > 0
# hidden in the last hour (check for false positives)
increase(threads_replies_total{action="hidden"}[1h]) > 0
# stuck
time() - threads_last_poll_success_timestamp_seconds > 1800
# token about to die (refresh failing)
threads_token_expiry_timestamp_seconds > 0 and threads_token_expiry_timestamp_seconds - time() < 7 * 86400
# API trouble
increase(threads_api_errors_total[15m]) > 0
```

To include the reply text in a notification, alert on the logs instead (for
example a VictoriaLogs or Loki query for `action:review`).

## Development

```bash
python -m venv .venv && .venv/bin/pip install -r requirements.txt
.venv/bin/python test_threads_monitor.py
```

## License

MIT
