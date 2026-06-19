# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the client-side rerank helper (no model needed — fake scorers)."""

from quiver import Match
from quiver.rerank import rerank


def _m(id: str, text: str) -> Match:
    return Match(id=id, score=0.0, payload={"text": text})


def test_rerank_reorders_by_batch_score_and_truncates():
    cands = [_m("a", "cat"), _m("b", "dogs are great"), _m("c", "x")]
    # Batch scorer: longer text scores higher.
    out = rerank("q", cands, lambda _q, texts: [len(t) for t in texts], top_k=2)
    assert [r.match.id for r in out] == ["b", "a"]
    assert out[0].score == len("dogs are great")


def test_rerank_pointwise_scorer():
    cands = [_m("a", "lo"), _m("b", "high score here")]
    out = rerank("q", cands, lambda _q, t: float(len(t)), batch=False)
    assert [r.match.id for r in out] == ["b", "a"]


def test_rerank_custom_text_extractor_and_empty():
    assert rerank("q", [], lambda _q, ts: []) == []
    cands = [Match(id="a", score=0.0, payload={"body": "z"}), Match(id="b", score=0.0, payload={"body": "zzz"})]
    out = rerank("q", cands, lambda _q, ts: [len(t) for t in ts], text_of=lambda m: m.payload["body"])
    assert [r.match.id for r in out] == ["b", "a"]


def test_rerank_rejects_mismatched_score_count():
    import pytest

    with pytest.raises(ValueError):
        rerank("q", [_m("a", "x")], lambda _q, ts: [1.0, 2.0])
