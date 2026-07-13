//! Fuzzy key suggestions — byte-faithful port of `review_kb/suggestions.py`.
//!
//! Parity note: Python uses `str.casefold()`; Rust uses `str::to_lowercase()`.
//! These are identical for ASCII (the realistic domain: rule keys are
//! `[A-Za-z0-9._-]`). They diverge only for a few non-ASCII code points
//! (e.g. German ß); covered later by the golden corpus if it surfaces any.

/// Standard iterative Levenshtein edit distance over `char`s.
pub fn levenshtein(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, left_char) in left.iter().enumerate() {
        let left_index = i + 1;
        let mut current: Vec<usize> = vec![left_index];
        for (j, right_char) in right.iter().enumerate() {
            let right_index = j + 1;
            let substitution_cost = if left_char == right_char { 0 } else { 1 };
            let value = (current[right_index - 1] + 1)
                .min(previous[right_index] + 1)
                .min(previous[right_index - 1] + substitution_cost);
            current.push(value);
        }
        previous = current;
    }
    previous[right.len()]
}

/// Rank-ordered candidate suggestions for `requested`, up to `limit`.
///
/// Sort key per candidate: `(category, levenshtein, original_index)` where
/// category is 0 (exact), 1 (either is a prefix of the other), 2 (else).
pub fn suggest_keys(requested: &str, available: &[String], limit: usize) -> Vec<String> {
    let folded = requested.to_lowercase();
    let mut indexed: Vec<(usize, &String)> = available.iter().enumerate().collect();
    indexed.sort_by(|&(_, a), &(_, b)| {
        rank(&folded, a).cmp(&rank(&folded, b))
    });
    indexed
        .iter()
        .take(limit)
        .map(|(_, s)| (*s).clone())
        .collect()
}

/// `(category, levenshtein, ordinal)` — ordinal folded in via the caller's
/// stable sort over enumerated candidates.
fn rank(folded: &str, candidate: &str) -> (u8, usize) {
    let candidate_folded = candidate.to_lowercase();
    let category = if candidate_folded == folded {
        0
    } else if candidate_folded.starts_with(folded) || folded.starts_with(&candidate_folded) {
        1
    } else {
        2
    };
    (category, levenshtein(folded, &candidate_folded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("flaw", "lawn"), 2);
    }

    #[test]
    fn suggest_matches_python_order() {
        // Captured from Python:
        //   suggest_keys("SEC-01", ["SEC-001","SEC-002","DB-004","SEC-010"])
        //   -> ['SEC-010', 'SEC-001', 'SEC-002']
        let available: Vec<String> = ["SEC-001", "SEC-002", "DB-004", "SEC-010"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(
            suggest_keys("SEC-01", &available, 3),
            vec!["SEC-010", "SEC-001", "SEC-002"]
        );
    }

    #[test]
    fn suggest_exact_then_prefix() {
        let available: Vec<String> = ["SEC-001", "SEC-010", "DB-004"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        // exact match wins (category 0)
        assert_eq!(suggest_keys("SEC-001", &available, 3)[0], "SEC-001");
    }

    #[test]
    fn suggest_respects_limit() {
        let available: Vec<String> = (0..5).map(|i| format!("K-00{i}")).collect();
        assert_eq!(suggest_keys("K", &available, 2).len(), 2);
    }
}
