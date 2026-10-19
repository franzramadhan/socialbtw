"""Hide unwanted replies on your own Threads posts.

Polls your recent posts, classifies new top-level replies with Cloudflare Clef,
optionally asks an OpenAI-compatible LLM about uncertain ones, and hides the
bad ones through the Threads API. Decisions go to stdout as JSON lines and to
Prometheus metrics.
"""

import json
import os
import re
import sqlite3
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request

from prometheus_client import Counter, Gauge, Histogram, start_http_server

THREADS_API = "https://graph.threads.net/v1.0"
BAD = ("hateful", "harassment", "spam", "sexual")

CRITERIA = {
    "hateful": "attacks a group or identity, slurs, dehumanizing language",
    "harassment": "personal attacks, insults, threats or mockery aimed at the post author or another person",
    "spam": "promotion, scams, crypto, links to unrelated sites, follow-for-follow, judi online or slot gambling promotion",
    "sexual": "unsolicited sexual content or sexual advances",
    "neutral": "on-topic reply, questions, disagreement or criticism without abuse",
    "positive": "supportive, thoughtful or adds value to the conversation",
}

LLM_PROMPT = """You moderate replies on the author's own Threads posts.
Replies may be in Indonesian, English, or a mix, and may use slang.
Hide replies that are hateful, harassment, spam (including judi online or slot
promotion), or unsolicited sexual content. Keep criticism, disagreement, and
friendly jokes. {extra}
Answer with JSON only: {{"action": "hide" | "keep" | "review", "reason": "<one short sentence>"}}.
Use "review" only when you truly cannot decide."""

REPLIES = Counter("threads_replies_total", "Processed replies", ["category", "action", "stage"])
BAD_PROB = Histogram("threads_clef_bad_probability", "Clef probability that a reply is bad",
                     buckets=(0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.95, 1.0))
API_ERRORS = Counter("threads_api_errors_total", "Failed outbound calls", ["endpoint"])
LAST_POLL = Gauge("threads_last_poll_success_timestamp_seconds", "Last successful poll")
TOKEN_EXPIRY = Gauge("threads_token_expiry_timestamp_seconds", "Threads token expiry, 0 if unknown")


def env(name, default=None):
    value = os.environ.get(name, default)
    if value is None:
        raise SystemExit(f"missing required env {name}")
    return value


def log(**fields):
    print(json.dumps({"ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), **fields},
                     ensure_ascii=False), flush=True)


def http(method, url, endpoint, data=None, headers=None):
    req = urllib.request.Request(
        url, method=method,
        data=json.dumps(data).encode() if data is not None else None,
        headers={"Content-Type": "application/json", **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.load(resp)
    except urllib.error.HTTPError as e:
        API_ERRORS.labels(endpoint).inc()
        # Error bodies help debugging; Threads and Cloudflare never echo the token.
        raise RuntimeError(f"{endpoint}: HTTP {e.code}: {e.read()[:500].decode(errors='replace')}") from None
    except Exception:
        API_ERRORS.labels(endpoint).inc()
        raise


class Store:
    """SQLite: processed replies (so each is classified once) and the refreshed token."""

    def __init__(self, path):
        self.db = sqlite3.connect(path)
        self.db.executescript("""
            CREATE TABLE IF NOT EXISTS replies (
                id TEXT PRIMARY KEY, post_id TEXT, username TEXT, text TEXT,
                category TEXT, bad_probability REAL, action TEXT, stage TEXT,
                reason TEXT, decided_at INTEGER);
            CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);
        """)

    def seen(self, reply_id):
        return self.db.execute("SELECT 1 FROM replies WHERE id = ?", (reply_id,)).fetchone() is not None

    def save(self, r):
        self.db.execute("INSERT OR REPLACE INTO replies VALUES (?,?,?,?,?,?,?,?,?,?)",
                        (r["reply_id"], r["post_id"], r["username"], r["text"], r["category"],
                         r["bad_probability"], r["action"], r["stage"], r["reason"], int(time.time())))
        self.db.commit()

    def get(self, key):
        row = self.db.execute("SELECT value FROM kv WHERE key = ?", (key,)).fetchone()
        return row[0] if row else None

    def put(self, key, value):
        self.db.execute("INSERT OR REPLACE INTO kv VALUES (?, ?)", (key, str(value)))
        self.db.commit()


class Token:
    """Long-lived Threads token. Refreshed daily and persisted, because it expires after 60 days.
    Changing THREADS_ACCESS_TOKEN replaces the stored token."""

    def __init__(self, store, seed):
        self.store = store
        if store.get("seed") != seed:
            store.put("seed", seed)
            store.put("token", seed)
            store.put("expires_at", 0)
        self.next_refresh = 0
        TOKEN_EXPIRY.set(float(store.get("expires_at")))

    @property
    def value(self):
        return self.store.get("token")

    def maybe_refresh(self):
        if time.time() < self.next_refresh:
            return
        self.next_refresh = time.time() + 86400
        q = urllib.parse.urlencode({"grant_type": "th_refresh_token", "access_token": self.value})
        try:
            resp = http("GET", f"https://graph.threads.net/refresh_access_token?{q}", "refresh")
        except Exception as e:
            # A token younger than 24h cannot be refreshed yet; the next attempt is tomorrow.
            log(level="warning", msg="token refresh failed", error=str(e))
            return
        expires_at = int(time.time()) + int(resp["expires_in"])
        self.store.put("token", resp["access_token"])
        self.store.put("expires_at", expires_at)
        TOKEN_EXPIRY.set(expires_at)
        log(level="info", msg="token refreshed", expires_at=expires_at)


def matches_tags(post, tags):
    """True when TAGS is empty, or the post's topic tag or a #hashtag in its text is in TAGS."""
    if not tags:
        return True
    found = {t.lower() for t in re.findall(r"#(\w+)", post.get("text", ""))}
    if post.get("topic_tag"):
        found.add(post["topic_tag"].lower().lstrip("#"))
    return not found.isdisjoint(tags)


def decide(probabilities, hide_threshold, review_threshold):
    """Map Clef category probabilities to (category, bad_probability, action).

    Bad probability is summed across bad categories, so a reply split between
    'hateful' and 'harassment' still counts as clearly bad."""
    bad_p = sum(probabilities.get(c, 0.0) for c in BAD)
    if bad_p >= hide_threshold:
        return max(BAD, key=lambda c: probabilities.get(c, 0.0)), bad_p, "hide"
    category = max(probabilities, key=probabilities.get)
    if bad_p >= review_threshold:
        return max(BAD, key=lambda c: probabilities.get(c, 0.0)), bad_p, "escalate"
    return category, bad_p, "keep"


class Monitor:
    def __init__(self):
        self.dry_run = env("DRY_RUN", "true").lower() != "false"
        self.interval = int(env("POLL_INTERVAL_SECONDS", "300"))
        self.lookback = int(env("LOOKBACK_DAYS", "3")) * 86400
        self.hide_threshold = float(env("HIDE_THRESHOLD", "0.85"))
        self.review_threshold = float(env("REVIEW_THRESHOLD", "0.5"))
        self.allowlist = {u.strip().lower().lstrip("@") for u in env("ALLOWLIST", "").split(",") if u.strip()}
        self.tags = {t.strip().lower().lstrip("#") for t in env("TAGS", "").split(",") if t.strip()}
        self.extra_rules = env("EXTRA_RULES", "")
        self.clef_url = env("CLEF_URL")
        self.clef_token = env("CLEF_TOKEN")
        self.clef_model = env("CLEF_MODEL", "clef")
        self.llm_url = env("LLM_BASE_URL", "").rstrip("/")
        self.llm_key = env("LLM_API_KEY", "")
        self.llm_model = env("LLM_MODEL", "")
        self.store = Store(env("DB_PATH", "threads-monitor.db"))
        self.token = Token(self.store, env("THREADS_ACCESS_TOKEN"))

    def threads(self, method, path, endpoint, **params):
        params["access_token"] = self.token.value
        return http(method, f"{THREADS_API}/{path}?{urllib.parse.urlencode(params)}", endpoint)

    def paged(self, path, endpoint, **params):
        page = self.threads("GET", path, endpoint, **params)
        while True:
            yield from page.get("data", [])
            nxt = page.get("paging", {}).get("next")
            if not nxt:
                return
            page = http("GET", nxt, endpoint)

    def clef(self, post, reply):
        resp = http("POST", self.clef_url, "clef", headers={"Authorization": f"Bearer {self.clef_token}"}, data={
            "model": self.clef_model,
            "state": {"post": post.get("text", ""), "reply": reply["text"], "reply_author": reply.get("username", "")},
            "questions": {"category": {
                "type": "choice",
                "instructions": "Classify the reply in the context of the post. "
                                "The reply may be in Indonesian, English, or mixed, and may use slang.",
                "criteria": CRITERIA}},
        })
        # Cloudflare wraps results in {"result": ...}; a pass-through proxy may not.
        return resp.get("result", resp)["answers"]["category"]["probabilities"]

    def llm(self, post, reply, probabilities):
        resp = http("POST", f"{self.llm_url}/chat/completions", "llm",
                    headers={"Authorization": f"Bearer {self.llm_key}"}, data={
            "model": self.llm_model,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": LLM_PROMPT.format(extra=self.extra_rules)},
                {"role": "user", "content": json.dumps({
                    "post": post.get("text", ""), "reply": reply["text"],
                    "reply_author": reply.get("username", ""), "classifier_probabilities": probabilities,
                }, ensure_ascii=False)},
            ],
        })
        content = resp["choices"][0]["message"]["content"]
        try:
            # Tolerate code fences or prose around the JSON object.
            out = json.loads(content[content.index("{"):content.rindex("}") + 1])
            action = out["action"] if out.get("action") in ("hide", "keep", "review") else "review"
            return action, str(out.get("reason", ""))
        except (ValueError, KeyError):
            return "review", f"unparseable LLM answer: {content[:200]}"

    def handle(self, post, reply):
        probabilities = self.clef(post, reply)
        category, bad_p, action = decide(probabilities, self.hide_threshold, self.review_threshold)
        BAD_PROB.observe(bad_p)
        stage, reason = "clef", ""
        if action == "escalate":
            if self.llm_url and self.llm_model:
                stage = "llm"
                action, reason = self.llm(post, reply, probabilities)
            else:
                action = "review"
        if action == "hide":
            if self.dry_run:
                action = "would_hide"
            else:
                self.threads("POST", f"{reply['id']}/manage_reply", "hide", hide="true")
                action = "hidden"
        elif action == "keep":
            action = "kept"
        record = {
            "reply_id": reply["id"], "post_id": post["id"], "username": reply.get("username", ""),
            "text": reply["text"], "category": category, "bad_probability": round(bad_p, 4),
            "action": action, "stage": stage, "reason": reason,
        }
        self.store.save(record)
        REPLIES.labels(category, action, stage).inc()
        log(level="info", msg="decision", dry_run=self.dry_run, permalink=reply.get("permalink", ""),
            probabilities=probabilities, **record)

    def poll(self):
        since = int(time.time()) - self.lookback
        for post in self.paged("me/threads", "posts", fields="id,text,timestamp,topic_tag", since=since):
            if not matches_tags(post, self.tags):
                continue
            replies = self.paged(f"{post['id']}/replies", "replies",
                                 fields="id,text,username,permalink,timestamp,hide_status,is_reply_owned_by_me")
            for reply in replies:
                # Skip your own replies, allowlisted users, image-only replies, replies already
                # handled, and anything not in its default state: a reply you unhid by hand
                # (UNHUSHED) is never hidden again.
                if (reply.get("is_reply_owned_by_me") or not reply.get("text")
                        or reply.get("username", "").lower() in self.allowlist
                        or reply.get("hide_status", "NOT_HUSHED") != "NOT_HUSHED"
                        or self.store.seen(reply["id"])):
                    continue
                try:
                    self.handle(post, reply)
                except Exception as e:
                    # One bad reply must not block the rest; it is retried next poll.
                    log(level="error", msg="reply failed", reply_id=reply["id"], error=str(e))

    def run(self):
        log(level="info", msg="starting", dry_run=self.dry_run, interval=self.interval, tags=sorted(self.tags),
            llm=bool(self.llm_url and self.llm_model))
        while True:
            self.token.maybe_refresh()
            try:
                self.poll()
                LAST_POLL.set_to_current_time()
            except Exception as e:
                log(level="error", msg="poll failed", error=str(e), trace=traceback.format_exc())
            time.sleep(self.interval)


if __name__ == "__main__":
    start_http_server(int(env("METRICS_PORT", "9090")))
    Monitor().run()
