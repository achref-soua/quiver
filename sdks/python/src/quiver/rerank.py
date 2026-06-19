# SPDX-License-Identifier: AGPL-3.0-only
"""Client-side reranking for the retrieve → rerank → generate RAG loop.

Quiver returns exact-reranked nearest neighbours; for higher precision you can
over-fetch and re-score the candidates with a stronger (and heavier) model — a
cross-encoder, an LLM judge, or any scorer — then keep the best. ``rerank`` is a
thin, model-agnostic helper for that step: you bring the scoring function, it
handles extracting the text, scoring, sorting, and truncation.

Example::

    from sentence_transformers import CrossEncoder
    ce = CrossEncoder("cross-encoder/ms-marco-MiniLM-L-6-v2")

    hits = client.search("kb", query_embedding, k=50)          # over-fetch
    top = rerank(question, hits, ce.predict, key="text", top_k=5)
"""

from __future__ import annotations

from typing import Any, Callable, Optional, Sequence, Union

from .client import Match

__all__ = ["rerank", "RerankResult"]


class RerankResult:
    """A reranked candidate: the original :class:`~quiver.Match` and its new score."""

    __slots__ = ("match", "score")

    def __init__(self, match: Match, score: float) -> None:
        self.match = match
        self.score = score

    def __repr__(self) -> str:
        return f"RerankResult(id={self.match.id!r}, score={self.score:.4f})"


# A scorer is either batch (query, [texts]) -> [scores] or pointwise
# (query, text) -> score; both are supported.
Scorer = Union[
    Callable[[str, list[str]], Sequence[float]],
    Callable[[str, str], float],
]


def rerank(
    query: str,
    candidates: Sequence[Match],
    score_fn: Scorer,
    *,
    key: str = "text",
    text_of: Optional[Callable[[Match], str]] = None,
    top_k: Optional[int] = None,
    batch: bool = True,
) -> list[RerankResult]:
    """Re-score ``candidates`` against ``query`` and return them best-first.

    ``score_fn`` is your reranker. By default it is treated as **batch** —
    ``score_fn(query, [text, …]) -> [score, …]`` (the shape most cross-encoders
    expose, e.g. ``CrossEncoder.predict([(q, t), …])`` via a small wrapper, or a
    function taking the query and a list of texts). Set ``batch=False`` for a
    **pointwise** ``score_fn(query, text) -> score``.

    The candidate text is ``match.payload[key]`` by default; override with
    ``text_of`` for a custom extractor. ``top_k`` truncates the result (``None``
    keeps all). Higher scores rank first. Ties keep the input order (stable sort).
    """
    if not candidates:
        return []

    def extract(m: Match) -> str:
        if text_of is not None:
            return text_of(m)
        payload = m.payload if isinstance(m.payload, dict) else {}
        value = payload.get(key)
        return value if isinstance(value, str) else ""

    texts = [extract(m) for m in candidates]

    if batch:
        scores = list(score_fn(query, texts))  # type: ignore[arg-type]
        if len(scores) != len(candidates):
            raise ValueError(
                f"score_fn returned {len(scores)} scores for {len(candidates)} candidates"
            )
    else:
        scores = [float(score_fn(query, t)) for t in texts]  # type: ignore[call-arg]

    ranked = sorted(
        (RerankResult(m, float(s)) for m, s in zip(candidates, scores)),
        key=lambda r: r.score,
        reverse=True,
    )
    return ranked if top_k is None else ranked[:top_k]
