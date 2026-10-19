# socialbtw

Hides unwanted replies on your own [Threads](https://www.threads.com) posts.

Every few minutes it reads new top-level replies on your recent posts and
has a **classifier** rate each one as `hateful`, `harassment`, `spam`,
`sexual`, `neutral` or `positive`. Clear cases are hidden through the official
Threads API. Uncertain cases can go to a **reviewer** (any OpenAI-compatible
chat model) for a second opinion, or be flagged for you to review.

The classifier is pluggable:

| `CLASSIFIER` | Models | |
|---|---|---|
| `systemone` (default) | Decision models in the System One format: [Cloudflare Clef](https://developers.cloudflare.com/workers-ai/models/clef/), TypeSafe Jev | Calibrated probabilities in one forward pass; fast and cheap. Directly or through [LiteLLM](https://docs.litellm.ai/docs/decisions) `/v1/systemone` |
| `chat` | Any OpenAI-compatible chat model: Claude or GPT through LiteLLM, Ollama, vLLM, ... | Asked for JSON probabilities. Slower and costlier; reasoning models can take over a minute per reply | Every decision is a JSON log line and a Prometheus metric, so you can
alert on it from Grafana or anything else.

Works with Indonesian, English and mixed replies. Spam includes product and
service promotion (even friendly, on-topic "use X, only Rp 99.000"), links and
domains (also obfuscated like "(dot)"), referral codes, and judi online or slot
promotion.

Written in Rust: one static binary in a 5 MB `scratch` image, about 1-2 MiB
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
new reply ──> classifier: probability per category
                │
   P(bad) >= HIDE_THRESHOLD ──────────────> hide
   P(bad) >= REVIEW_THRESHOLD ──> reviewer set? ── yes ──> hide / keep / review
                │                         └─ no ────────> review
   otherwise ─────────────────────────────> keep
```

`P(bad)` is the sum of the four bad categories. Each reply is decided once and
remembered in the database (a SQLite file, or PostgreSQL with `DATABASE_URL`).
A reply you unhide by hand is never hidden again.

**Start in dry-run** (the default). Decisions are logged as `would_hide` and
nothing changes on Threads. Look at the `threads_bad_probability`
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
account.

**Or register the token from the browser** instead of the steps above: set
`THREADS_APP_ID`, `THREADS_APP_SECRET` and `OAUTH_REDIRECT_URI`, add the same
redirect URI under the app's Threads settings (Redirect Callback URLs), and
open `/oauth/start` on the app. It sends you to Threads to approve, takes the
code on `/oauth/callback`, exchanges it for a long-lived token and stores it.
`THREADS_ACCESS_TOKEN` is then optional. The redirect only happens in your
browser, so the app can stay private (LAN or VPN); it needs HTTPS. Set
`THREADS_USERNAME` so a token for any other account is rejected. The token is refreshed every 24 hours and stored in the database, so
it does not expire while the app runs. Setting a different
`THREADS_ACCESS_TOKEN` replaces the stored one.

### 2. Classifier

**Clef on Cloudflare (default):** create a Cloudflare API token with
**Workers AI: Read** and note your account ID. Clef's free tier (10,000
Neurons/day) covers a typical personal account.

```bash
CLASSIFIER_URL=https://api.cloudflare.com/client/v4/accounts/<account-id>/ai/run/@cf/cloudflare/clef
CLASSIFIER_TOKEN=<cloudflare token>
```

**Through [LiteLLM](https://docs.litellm.ai) (v1.104.2 or newer)**: add the
decision model to the model list and use LiteLLM's System One route, so one
virtual key (with its budget and spend tracking) covers the classifier and the
reviewer:

```yaml
model_list:
  - model_name: clef
    litellm_params: {model: cloudflare/@cf/cloudflare/clef, api_key: os.environ/CLOUDFLARE_API_KEY}
```

```bash
CLASSIFIER_URL=http://litellm:4000/v1/systemone
CLASSIFIER_TOKEN=<virtual key>   # same key as LLM_API_KEY
CLASSIFIER_MODEL=clef            # the model_name above (or jev, clef-flash, ...)
```

**A chat model** (for example Claude through LiteLLM):

```bash
CLASSIFIER=chat
CLASSIFIER_URL=http://litellm:4000/v1   # OpenAI-compatible base URL
CLASSIFIER_TOKEN=<key>
CLASSIFIER_MODEL=claude-haiku-4-5
```

### 3. Run

```bash
docker run -d --name socialbtw \
  -e THREADS_ACCESS_TOKEN=... \
  -e CLASSIFIER_URL=https://api.cloudflare.com/client/v4/accounts/<account-id>/ai/run/@cf/cloudflare/clef \
  -e CLASSIFIER_TOKEN=... \
  -v socialbtw:/data -p 9090:9090 \
  ghcr.io/franzramadhan/socialbtw:main
```

Without Docker: `cargo build --release && ./target/release/socialbtw`.

## Configuration

| Env | Default | |
|-----|---------|---|
| `THREADS_ACCESS_TOKEN` | required without OAuth | Long-lived Threads token |
| `THREADS_APP_ID` | empty | Threads app ID; with the next two, enables `/oauth/start` and `/oauth/callback` |
| `THREADS_APP_SECRET` | empty | Threads app secret |
| `OAUTH_REDIRECT_URI` | empty | Public or private HTTPS URL that reaches `/oauth/callback`, registered in the app |
| `THREADS_USERNAME` | empty | Only accept tokens for this account (recommended with OAuth) |
| `CLASSIFIER` | `systemone` | `systemone` (Clef, Jev) or `chat` (OpenAI-compatible chat model) |
| `CLASSIFIER_URL` | required | `systemone`: the full endpoint, Cloudflare `.../ai/run/@cf/cloudflare/clef` or LiteLLM `.../v1/systemone`. `chat`: the base URL, e.g. `http://litellm:4000/v1` |
| `CLASSIFIER_TOKEN` | empty | Bearer token for `CLASSIFIER_URL` |
| `CLASSIFIER_MODEL` | `clef` for `systemone`, required for `chat` | e.g. `clef` (27B, more accurate), `clef-flash` (9B, cheaper), `jev`; behind LiteLLM, the `model_name` |
| `LLM_BASE_URL` | empty | Reviewer: OpenAI-compatible base URL, e.g. `http://litellm:4000/v1`. Empty: uncertain replies go to `review` |
| `LLM_API_KEY` | empty | Reviewer API key |
| `LLM_MODEL` | empty | Reviewer model; a small cheap one is enough |
| `EXTRA_RULES` | empty | Your own policy, added to the chat prompts (chat classifier and reviewer), e.g. `Mild swearing between friends is fine.` |
| `HTTP_TIMEOUT_SECONDS` | `120` | Per outbound call; raise it for slow reasoning models |
| `DRY_RUN` | `true` | `false` to actually hide replies |
| `HIDE_THRESHOLD` | `0.85` | P(bad) at or above this hides |
| `REVIEW_THRESHOLD` | `0.5` | P(bad) at or above this escalates |
| `TOPICS` | empty | Comma-separated Threads topics (the topic shown in the post header, e.g. `Home Lab,Rust`); posts with one of these topics are monitored. Case-insensitive |
| `HASHTAGS` | empty | Comma-separated hashtags (`#` optional); posts with one of these `#hashtags` in the text are monitored. Case-insensitive. With both `TOPICS` and `HASHTAGS` empty, all posts are monitored; with either set, a post matching either one is monitored |
| `ALLOWLIST` | empty | Comma-separated usernames never touched |
| `POLL_INTERVAL_SECONDS` | `300` | |
| `POST_CHECK_SCHEDULE` | `1d:5m,7d:1h,30d:6h` | How often each post's replies are read, by age of its last activity (the post or its newest reply): under 1 day every 5 min, under 7 days hourly, older every 6 h. Saves Threads API calls on quiet posts; a new reply makes a post "young" again. In memory: after a restart every post is read once |
| `LOOKBACK_DAYS` | `3` | How far back to read your posts (reposts excluded). Each poll costs 1 call per own post; keep it small if you post a lot |
| `DB_PATH` | `/data/socialbtw.db` in the image, `socialbtw.db` otherwise | SQLite file, used when `DATABASE_URL` is empty. Holds the refreshed token: keep it private |
| `DATABASE_URL` | empty | PostgreSQL instead of SQLite, e.g. `postgres://user:pass@host:5432/socialbtw?sslmode=require`. Tables are created on first connect; the connection is re-opened after a failover. Run one instance only |
| `DATABASE_CA_FILE` | empty | PEM CA for the PostgreSQL server certificate (private CA). Empty: public CA roots. The certificate and hostname are always verified |
| `METRICS_PORT` | `9090` | HTTP port: `/metrics`, and `/oauth/*` when OAuth is set. Expose only `/oauth/` |

## Observability

Logs are one JSON object per line on stdout, with `ts` in Unix seconds. Each
decision has `msg=decision`
plus `reply_id`, `username`, `text`, `category`, `bad_probability`,
`probabilities`, `action` (`hidden`, `would_hide`, `kept`, `review`), `stage`
(`classifier` or `reviewer`), `reason` and `permalink`.

| Metric | |
|--------|---|
| `threads_replies_total{category,action,stage}` | Decisions |
| `threads_bad_probability` | Histogram of P(bad), for tuning thresholds |
| `threads_post_checks_total{result}` | Posts whose replies were read (`checked`) or skipped by `POST_CHECK_SCHEDULE` (`skipped`) |
| `threads_api_errors_total{endpoint}` | Failed calls: `posts`, `replies`, `hide`, `refresh`, `oauth`, `classifier`, `reviewer` |
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
# also against PostgreSQL:
TEST_DATABASE_URL=postgres://... DATABASE_CA_FILE=ca.crt cargo test
# also against a chat model:
TEST_CHAT_URL=http://litellm:4000/v1 TEST_CHAT_MODEL=... TEST_CHAT_KEY=... cargo test live_chat
```

Code map (`src/`):

| File | |
|---|---|
| `main.rs` | wiring and signal handling |
| `monitor.rs` | poll loop: which posts and replies, what to do with each |
| `classifier.rs` | categories, the `Classifier` trait, `systemone` and `chat` backends, thresholds |
| `reviewer.rs` | second opinion on uncertain replies |
| `llm.rs` | OpenAI-compatible chat client |
| `threads.rs` | Threads API: token, posts, replies, hide |
| `oauth.rs` | token registration from the browser |
| `server.rs` | `/metrics`, `/oauth/*` |
| `store.rs` | SQLite or PostgreSQL |
| `metrics.rs`, `http.rs`, `util.rs` | metrics, outbound HTTP, helpers |

## License

MIT
