import os
import tempfile

import threads_monitor as tm


def test_decide():
    assert tm.decide({"spam": 0.95, "neutral": 0.05}, 0.85, 0.5) == ("spam", 0.95, "hide")
    # Split across bad categories still sums to a hide.
    cat, p, action = tm.decide({"hateful": 0.45, "harassment": 0.44, "neutral": 0.11}, 0.85, 0.5)
    assert (cat, action) == ("hateful", "hide") and abs(p - 0.89) < 1e-9
    assert tm.decide({"harassment": 0.6, "neutral": 0.4}, 0.85, 0.5)[2] == "escalate"
    assert tm.decide({"positive": 0.9, "spam": 0.1}, 0.85, 0.5) == ("positive", 0.1, "keep")


def test_matches_tags():
    assert tm.matches_tags({"text": "hi"}, set())
    assert tm.matches_tags({"text": "hi", "topic_tag": "Homelab"}, {"homelab"})
    assert tm.matches_tags({"text": "new post #K8s today"}, {"k8s", "golang"})
    assert not tm.matches_tags({"text": "no tags", "topic_tag": "cooking"}, {"homelab"})


def run_handle(dry_run, clef_probs, llm_answer=None):
    calls = []

    def fake_http(method, url, endpoint, data=None, headers=None):
        calls.append(endpoint)
        if endpoint == "clef":
            return {"result": {"answers": {"category": {"probabilities": clef_probs}}}}
        if endpoint == "llm":
            return {"choices": [{"message": {"content": llm_answer}}]}
        return {"success": True}

    tm.http = fake_http
    os.environ.update(THREADS_ACCESS_TOKEN="t", CLEF_URL="http://clef", CLEF_TOKEN="c",
                      DRY_RUN=str(dry_run).lower(), DB_PATH=tempfile.mktemp(),
                      LLM_BASE_URL="http://llm" if llm_answer else "", LLM_MODEL="m" if llm_answer else "")
    m = tm.Monitor()
    m.handle({"id": "p1", "text": "post"}, {"id": "r1", "text": "slot gacor", "username": "x"})
    action = m.store.db.execute("SELECT action FROM replies WHERE id = 'r1'").fetchone()[0]
    return action, calls


def test_handle():
    assert run_handle(True, {"spam": 0.99}) == ("would_hide", ["clef"])
    assert run_handle(False, {"spam": 0.99}) == ("hidden", ["clef", "hide"])
    assert run_handle(False, {"spam": 0.6, "neutral": 0.4}) == ("review", ["clef"])
    assert run_handle(False, {"spam": 0.6, "neutral": 0.4},
                      '```json\n{"action": "hide", "reason": "gambling promo"}\n```') == ("hidden", ["clef", "llm", "hide"])
    assert run_handle(False, {"spam": 0.6, "neutral": 0.4}, "no idea") == ("review", ["clef", "llm"])


if __name__ == "__main__":
    test_decide()
    test_matches_tags()
    test_handle()
    print("ok")
