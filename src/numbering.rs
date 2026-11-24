//! Episode numbering parity with the Python original.
//!
//! AniWorld and SerienStream keep per-season indices in their URLs, but long
//! running series expose the absolute number in the English title, e.g.
//! `[Episode 1157]` or `[JP Episode 1157]`. Saving those as `S21E001` would be
//! wrong, so the title marker wins and the seasonal index is only a fallback.
//!
//! Set `ANIWORLD_SEASON_EPISODE_NUMBERS=1` to keep the seasonal index instead,
//! matching the original's opt-out.

use std::env;

use regex::Regex;

pub const SEASON_EPISODE_NUMBERS_ENV: &str = "ANIWORLD_SEASON_EPISODE_NUMBERS";

/// `[Episode 12]`, `[JP Episode 12]`, or the whole title being `Episode 12`.
const MARKER: &str =
    r"(?i)\[\s*(?:JP\s+)?Episode\s+([0-9]+)\s*\]|^\s*(?:JP\s+)?Episode\s+([0-9]+)\s*$";

/// Read an explicit marker, never an unrelated number in a title.
pub fn number_from_title(title: &str) -> Option<u32> {
    let regex = Regex::new(MARKER).ok()?;
    let mut numbers: Vec<u32> = regex
        .captures_iter(title)
        .filter_map(|captures| {
            captures
                .get(1)
                .or_else(|| captures.get(2))
                .and_then(|m| m.as_str().parse::<u32>().ok())
        })
        .filter(|number| *number > 0)
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    if numbers.len() == 1 {
        numbers.pop()
    } else {
        None
    }
}

/// True when the user asked to keep the site's per-season index.
pub fn keep_seasonal_numbers() -> bool {
    env::var(SEASON_EPISODE_NUMBERS_ENV)
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
}

/// The number used in file names and queue labels.
pub fn effective_number(
    title_en: Option<&str>,
    is_movie: bool,
    seasonal_index: u32,
    keep_seasonal: bool,
) -> u32 {
    if is_movie || keep_seasonal {
        return seasonal_index;
    }
    title_en
        .and_then(number_from_title)
        .unwrap_or(seasonal_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bracketed_markers() {
        assert_eq!(
            number_from_title("The Truth About That Day [Episode 1157]"),
            Some(1157)
        );
        assert_eq!(number_from_title("[JP Episode 12] Title"), Some(12));
        assert_eq!(number_from_title("[Episode 3]"), Some(3));
        assert_eq!(number_from_title("[Episode 0]"), None);
    }

    #[test]
    fn reads_standalone_titles_only() {
        assert_eq!(number_from_title("Episode 7"), Some(7));
        assert_eq!(number_from_title("JP Episode 42"), Some(42));
        // A number inside prose is not a marker.
        assert_eq!(number_from_title("Episode 7 Recap"), None);
        assert_eq!(number_from_title("The 7th Episode"), None);
    }

    #[test]
    fn conflicting_markers_are_ignored() {
        assert_eq!(number_from_title("[Episode 5] vs [Episode 6]"), None);
        assert_eq!(number_from_title("[Episode 5] again [Episode 5]"), Some(5));
    }

    #[test]
    fn effective_number_prefers_title_over_season_index() {
        assert_eq!(
            effective_number(Some("[Episode 1157] Egghead"), false, 21, false),
            1157
        );
        // No marker: seasonal fallback.
        assert_eq!(
            effective_number(Some("The Fate of Particular Adventurers"), false, 2, false),
            2
        );
        // Opt out keeps the seasonal index, as in the Python original.
        assert_eq!(
            effective_number(Some("[Episode 1157] Egghead"), false, 21, true),
            21
        );
        // Movies never take a title marker.
        assert_eq!(
            effective_number(Some("[Episode 3] Movie"), true, 1, false),
            1
        );
        assert_eq!(effective_number(None, false, 4, false), 4);
    }
}
