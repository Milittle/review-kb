from __future__ import annotations


def _levenshtein(left: str, right: str) -> int:
    previous = list(range(len(right) + 1))
    for left_index, left_character in enumerate(left, start=1):
        current = [left_index]
        for right_index, right_character in enumerate(right, start=1):
            current.append(
                min(
                    current[-1] + 1,
                    previous[right_index] + 1,
                    previous[right_index - 1] + (left_character != right_character),
                )
            )
        previous = current
    return previous[-1]


def suggest_keys(requested: str, available: list[str], limit: int = 3) -> list[str]:
    folded = requested.casefold()

    def rank(item: tuple[int, str]) -> tuple[int, int, int]:
        ordinal, candidate = item
        candidate_folded = candidate.casefold()
        if candidate_folded == folded:
            category = 0
        elif candidate_folded.startswith(folded) or folded.startswith(candidate_folded):
            category = 1
        else:
            category = 2
        return category, _levenshtein(folded, candidate_folded), ordinal

    ranked = sorted(enumerate(available), key=rank)
    return [candidate for _, candidate in ranked[:limit]]
