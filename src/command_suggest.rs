use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Suggestion {
    name: String,
    normalized: String,
    distance: usize,
    starts_match: bool,
    subsequence_match: bool,
    char_overlap: usize,
    common_prefix: usize,
    length_diff: usize,
}

pub fn suggest_commands<I, S>(query: &str, candidates: I, limit: usize) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if limit == 0 {
        return Vec::new();
    }

    let normalized_query = normalize(query);
    if normalized_query.is_empty() {
        return Vec::new();
    }
    let max_distance = max_distance(normalized_query.chars().count());

    let mut seen = HashSet::new();
    let mut suggestions = Vec::new();
    for candidate in candidates {
        let candidate = candidate.as_ref().trim();
        let normalized = normalize(candidate);
        if !is_useful_candidate(candidate, &normalized) || normalized == normalized_query {
            continue;
        }
        if !seen.insert(normalized.clone()) {
            continue;
        }

        let distance = damerau_levenshtein(&normalized_query, &normalized);
        let starts_match =
            normalized.starts_with(&normalized_query) || normalized_query.starts_with(&normalized);
        let subsequence_match = is_subsequence(&normalized_query, &normalized)
            || is_subsequence(&normalized, &normalized_query);
        let char_overlap = shared_char_count(&normalized_query, &normalized);
        let common_prefix = common_prefix_len(&normalized_query, &normalized);
        let length_diff = normalized_query
            .chars()
            .count()
            .abs_diff(normalized.chars().count());

        if distance > max_distance && !starts_match && !subsequence_match && common_prefix < 2 {
            continue;
        }

        suggestions.push(Suggestion {
            name: candidate.to_string(),
            normalized,
            distance,
            starts_match,
            subsequence_match,
            char_overlap,
            common_prefix,
            length_diff,
        });
    }

    suggestions.sort_by(|a, b| {
        a.distance
            .cmp(&b.distance)
            .then_with(|| b.starts_match.cmp(&a.starts_match))
            .then_with(|| b.char_overlap.cmp(&a.char_overlap))
            .then_with(|| b.common_prefix.cmp(&a.common_prefix))
            .then_with(|| b.subsequence_match.cmp(&a.subsequence_match))
            .then_with(|| a.length_diff.cmp(&b.length_diff))
            .then_with(|| a.normalized.cmp(&b.normalized))
    });

    suggestions
        .into_iter()
        .take(limit)
        .map(|s| s.name)
        .collect()
}

fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

fn is_useful_candidate(candidate: &str, normalized: &str) -> bool {
    !candidate.is_empty()
        && !normalized.is_empty()
        && !candidate.starts_with('_')
        && normalized.chars().any(|c| c.is_ascii_alphanumeric())
        && normalized
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.'))
}

fn max_distance(query_len: usize) -> usize {
    match query_len {
        0..=4 => 1,
        5..=8 => 2,
        _ => 3,
    }
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(a, b)| a == b).count()
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut needle = needle.chars();
    let mut current = needle.next();
    if current.is_none() {
        return true;
    }
    for ch in haystack.chars() {
        if Some(ch) == current {
            current = needle.next();
            if current.is_none() {
                return true;
            }
        }
    }
    false
}

fn shared_char_count(a: &str, b: &str) -> usize {
    let mut counts = [0u8; 128];
    for ch in a.bytes() {
        if (ch as usize) < counts.len() {
            counts[ch as usize] = counts[ch as usize].saturating_add(1);
        }
    }

    let mut shared = 0usize;
    for ch in b.bytes() {
        if (ch as usize) < counts.len() && counts[ch as usize] > 0 {
            counts[ch as usize] -= 1;
            shared += 1;
        }
    }
    shared
}

fn damerau_levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];

    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in dp[0].iter_mut().enumerate() {
        *cell = j;
    }

    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let substitution_cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + substitution_cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(dp[i - 2][j - 2] + 1);
            }
            dp[i][j] = best;
        }
    }

    dp[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_closest_command_first() {
        let suggestions =
            suggest_commands("cargu", ["cargo", "curl", "cargo-clippy", "cargo-fmt"], 3);
        assert_eq!(suggestions[0], "cargo");
        assert_eq!(suggestions.len(), 3);
    }

    #[test]
    fn handles_transposed_letters() {
        let suggestions = suggest_commands("gti", ["git", "go", "grep"], 3);
        assert_eq!(suggestions, vec!["git"]);
    }

    #[test]
    fn filters_unhelpful_internal_names() {
        let suggestions = suggest_commands("josn", ["_json", "[", "json", "join"], 3);
        assert_eq!(suggestions[0], "json");
    }

    #[test]
    fn respects_limit_after_sorting() {
        let suggestions = suggest_commands("nodee", ["node", "nodejs", "nodenv", "npm"], 2);
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0], "node");
    }
}
