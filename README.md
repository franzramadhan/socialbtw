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

Written in Rust: one static binary in a 4.4 MB `scratch` image, about 1-2 MiB
of memory at runtime.

## What it does not do

- **Block users or like replies.** The Threads API has no endpoint for either.
- **Delete replies.** Hiding is the strongest action the API allows.
- **Nested replies.** Only top-level replies can be hidden; hiding one also
  hides everything under it.
- **Image-only replies.** Replies without text are skipped.
- **Reposts.** Replies under a post you reposted belong to its author; reposts are skipped.

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

**Through [LiteLLM](https://docs.litellm.ai) (v1.104.2 or newer)** instead of
calling Cloudflare directly: add Clef to the model list and use LiteLLM's
System One route, so one virtual key (with its budget and spend tracking)
covers both Clef and the LLM:

```yaml
model_list:
  - model_name: clef
    litellm_params: {model: cloudflare/@cf/cloudflare/clef, api_key: os.environ/CLOUDFLARE_API_KEY}
```

```bash
CLEF_URL=http://litellm:4000/v1/systemone
CLEF_TOKEN=<virtual key>   # same key as LLM_API_KEY
CLEF_MODEL=clef            # the model_name above
```

### 3. Run

```bash
docker run -d --name socialbtw \
  -e THREADS_ACCESS_TOKEN=... \
  -e CLEF_URL=https://api.cloudflare.com/client/v4/accounts/<account-id>/ai/run/@cf/cloudflare/clef \
  -e CLEF_TOKEN=... \
  -v socialbtw:/data -p 9090:9090 \
  ghcr.io/franzramadhan/socialbtw:main
```

Without Docker: `cargo build --release && ./target/release/socialbtw`.

## Configuration

| Env | Default | |
|-----|---------|---|
| `THREADS_ACCESS_TOKEN` | required | Long-lived Threads token |
| `CLEF_URL` | required | Clef endpoint: Cloudflare `.../ai/run/@cf/cloudflare/clef` or LiteLLM `.../v1/systemone` |
| `CLEF_TOKEN` | required | Bearer token for `CLEF_URL` |
| `CLEF_MODEL` | `clef` | `clef` (27B, more accurate) or `clef-flash` (9B, cheaper); behind LiteLLM, the `model_name` |
| `LLM_BASE_URL` | empty | OpenAI-compatible base URL, e.g. `http://litellm:4000/v1`. Empty: uncertain replies go to `review` |
| `LLM_API_KEY` | empty | API key for the LLM |
| `LLM_MODEL` | empty | Model name; a small cheap one is enough |
| `EXTRA_RULES` | empty | Your own policy, appended to the LLM prompt (e.g. `Mild swearing between friends is fine.`) |
| `DRY_RUN` | `true` | `false` to actually hide replies |
| `HIDE_THRESHOLD` | `0.85` | P(bad) at or above this hides |
| `REVIEW_THRESHOLD` | `0.5` | P(bad) at or above this escalates |
| `TOPICS` | empty | Comma-separated Threads topics (the topic shown in the post header, e.g. `Home Lab,Rust`); posts with one of these topics are monitored. Case-insensitive |
| `HASHTAGS` | empty | Comma-separated hashtags (`#` optional); posts with one of these `#hashtags` in the text are monitored. Case-insensitive. With both `TOPICS` and `HASHTAGS` empty, all posts are monitored; with either set, a post matching either one is monitored |
| `ALLOWLIST` | empty | Comma-separated usernames never touched |
| `POLL_INTERVAL_SECONDS` | `300` | |
| `LOOKBACK_DAYS` | `3` | How far back to read your posts (reposts excluded). Each poll costs 1 call per own post; keep it small if you post a lot |
| `DB_PATH` | `/data/socialbtw.db` in the image, `socialbtw.db` otherwise | SQLite file. Holds the refreshed token: keep it private |
| `METRICS_PORT` | `9090` | Prometheus `/metrics` |

## Observability

Logs are one JSON object per line on stdout, with `ts` in Unix seconds. Each
decision has `msg=decision`
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
cargo test
```

## License

MIT
