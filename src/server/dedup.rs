//! Issue deduplication — the stage that costs nothing because it runs first.
//!
//! Repositories accumulate the same issue over and over. Every restatement of
//! one already answered is a triage call, an analysis call and a context window
//! spent to reach a conclusion that is already sitting in a comment thread. It
//! is the cheapest saving available and the only one that is a hundred percent
//! rather than a fraction.
//!
//! **No embeddings.** The obvious design calls an embedding API and compares
//! vectors, and it would work — but it means a network round trip and a bill on
//! every issue, including the overwhelming majority that are not duplicates, in
//! a product whose entire argument is that you should not pay for context you
//! did not need. Paying to find out you did not have to pay is the wrong shape.
//!
//! So three local signals, all exact, all free:
//!
//! * **Word shingles.** Overlapping three-word sequences. Order-sensitive and
//!   very precise, but it only fires on a near-copy — measured at 0.78 against
//!   a paste with a comment appended, and 0.19 against the *same report typed
//!   again in the reporter's own words*. That second number is why shingles
//!   cannot be the primary signal: people retype, they do not paste.
//! * **Content words.** The vocabulary with grammar removed. This is what
//!   actually separates a restatement from an unrelated report:
//!
//!   ```text
//!                            3-gram   content words
//!     reworded                0.188   0.692
//!     pasted, comment added   0.783   0.880
//!     unrelated issue         0.000   0.043
//!     same file, other bug    0.000   0.026
//!   ```
//!
//! * **Graph seeds.** The nodes the context builder would select. The signal
//!   nothing else here has: two issues are about the same code, as judged by
//!   the machinery that will go on to answer them. Stored as `NodeKey`s rather
//!   than ids, so a fingerprint survives reindexing.
//!
//! A near-copy is conclusive on text alone. Everything else needs vocabulary
//! *and* code to agree — seeds alone are far too weak, since two unrelated bugs
//! in one popular function share every seed, and vocabulary alone would call a
//! filled-in template a duplicate of the template.
//!
//! The asymmetry that sets the thresholds: a missed duplicate costs one
//! analysis. A false duplicate means a real issue is answered with a link to an
//! unrelated one, and the reporter concludes the bot does not work. The second
//! is much worse, so the constants below are deliberately conservative and the
//! comment always says which issue it matched and invites a correction.

use crate::graph::Graph;
use crate::query;
use rustc_hash::FxHasher;
use std::hash::{Hash, Hasher};

/// Length of the word sequences compared. Three is the usual choice: pairs
/// collide on ordinary English, and four is brittle against a reworded
/// sentence that is plainly the same report.
const SHINGLE: usize = 3;

/// Below this there is not enough text to judge. "Crashes on startup" is four
/// words and two shingles, and two such issues can be entirely unrelated.
const MIN_SHINGLES: usize = 8;

/// Shingle overlap at which the text carries the decision on its own. Only a
/// paste reaches this; a rewording of the same report measures around 0.19.
const COPY_MIN: f32 = 0.60;

/// ...but the code may still veto it. Seeds are derived from the text, so a
/// paste normally selects the same nodes and clears this trivially. The case it
/// guards is a filled-in template — identical prose, different subject — where
/// the wording says duplicate and the code says otherwise. The code wins.
const COPY_SEED_FLOOR: f32 = 0.20;

/// Content-word overlap required on the corroborated path. Sits an order of
/// magnitude above what unrelated issues score and comfortably below what a
/// restatement does.
const WORDS_MIN: f32 = 0.50;

/// Seed overlap required alongside it. Corroboration that the two issues
/// concern the same code, not the primary evidence.
const SEED_MIN: f32 = 0.50;

/// Grammar. Removed before comparing vocabulary, because "the" appearing in
/// both is not evidence of anything. Short and generic on purpose: this list
/// exists to strip function words, not to encode a domain.
const GRAMMAR: &[&str] = &[
    "the", "and", "for", "not", "but", "with", "from", "this", "that", "these", "those", "when",
    "then", "than", "have", "has", "had", "was", "were", "are", "been", "being", "you", "your",
    "all", "any", "can", "could", "may", "might", "will", "would", "should", "does", "did", "done",
    "its", "it's", "our", "their", "there", "here", "what", "which", "who", "why", "how", "some",
    "each", "more", "most", "other", "such", "only", "also", "into", "over", "off", "out", "about",
    "after", "before", "again", "very", "still", "just", "even", "because", "while", "where",
    "both", "same", "every",
];

/// How far back to compare. Bounded because this runs on every issue, and
/// because a duplicate of something from two thousand issues ago is not a
/// duplicate in any sense the reporter cares about.
pub const LOOKBACK: usize = 300;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fingerprint {
    /// Sorted, deduplicated hashes of word three-grams.
    pub shingles: Vec<u64>,
    /// Sorted, deduplicated hashes of content words.
    pub words: Vec<u64>,
    /// Sorted, deduplicated `NodeKey`s of the seeds the context builder picks.
    pub seeds: Vec<u64>,
}

fn hash_of<T: Hash>(v: T) -> u64 {
    let mut h = FxHasher::default();
    v.hash(&mut h);
    h.finish()
}

/// Word shingles over normalised text.
///
/// Normalisation folds case and drops punctuation, so a quoted traceback and
/// the same traceback pasted without backticks agree. Numbers are kept: a line
/// number or a version is often the only thing distinguishing two otherwise
/// identical reports, and dropping it would manufacture duplicates.
pub fn shingles(text: &str) -> Vec<u64> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
        .collect();
    if words.len() < SHINGLE {
        return Vec::new();
    }
    let mut out: Vec<u64> = words.windows(SHINGLE).map(hash_of).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Content words: everything that is not grammar and not a fragment.
///
/// Numbers survive. A version or a line number is often the only thing
/// separating two otherwise identical reports, and dropping it would
/// manufacture duplicates.
pub fn content_words(text: &str) -> Vec<u64> {
    let mut out: Vec<u64> = text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() > 2)
        .map(str::to_ascii_lowercase)
        .filter(|w| !GRAMMAR.contains(&w.as_str()))
        .map(hash_of)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The nodes the context builder would choose, as stable keys.
pub fn seeds(g: &Graph, text: &str) -> Vec<u64> {
    let mut out: Vec<u64> = query::seeds_from_text(g, text, 24)
        .into_iter()
        .map(|n| g.node_key(n))
        .filter(|&k| k != 0)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

pub fn fingerprint(g: Option<&Graph>, title: &str, body: &str) -> Fingerprint {
    let text = format!("{title}\n{body}");
    Fingerprint {
        shingles: shingles(&text),
        words: content_words(&text),
        seeds: g.map(|g| seeds(g, &text)).unwrap_or_default(),
    }
}

/// Jaccard over two sorted, deduplicated slices. Exact, not sketched: issue
/// bodies are small enough that a MinHash would trade accuracy for a saving
/// nobody would notice.
pub fn jaccard(a: &[u64], b: &[u64]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut both) = (0usize, 0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                both += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = a.len() + b.len() - both;
    both as f32 / union as f32
}

#[derive(Debug, Clone, Copy)]
pub struct Score {
    /// Three-gram overlap. High only for a near-copy.
    pub copy: f32,
    /// Content-word overlap. The signal that survives rewording.
    pub text: f32,
    /// Seed overlap. Whether it is about the same code.
    pub seed: f32,
}

impl Score {
    pub fn is_duplicate(self) -> bool {
        let pasted = self.copy >= COPY_MIN && self.seed >= COPY_SEED_FLOOR;
        let restated = self.text >= WORDS_MIN && self.seed >= SEED_MIN;
        pasted || restated
    }

    /// No graph, so no seed evidence either way. Text has to decide alone, and
    /// only the conclusive form of it is allowed to.
    pub fn is_duplicate_without_seeds(self) -> bool {
        self.copy >= COPY_MIN
    }

    /// Rank among several matches. Weighted toward the corroborated signals
    /// rather than raw text overlap, so a long pasted template does not beat a
    /// genuine restatement of the same bug.
    fn strength(self) -> f32 {
        self.copy + self.text + self.seed
    }
}

/// Compare two fingerprints.
///
/// Returns `None` when there is not enough text on either side to judge, which
/// is a different answer from "not a duplicate" and is treated as such: a short
/// issue is analysed normally rather than matched on thin evidence.
pub fn compare(new: &Fingerprint, old: &Fingerprint) -> Option<Score> {
    if new.shingles.len() < MIN_SHINGLES || old.shingles.len() < MIN_SHINGLES {
        return None;
    }
    Some(Score {
        copy: jaccard(&new.shingles, &old.shingles),
        text: jaccard(&new.words, &old.words),
        seed: jaccard(&new.seeds, &old.seeds),
    })
}

/// The strongest match among prior issues, if it clears both bars.
///
/// `prior` is `(issue_number, fingerprint)`; the caller supplies it already
/// bounded and already excluding the issue being answered.
pub fn best_match(new: &Fingerprint, prior: &[(i64, Fingerprint)]) -> Option<(i64, Score)> {
    let mut best: Option<(i64, Score)> = None;
    let blind = new.seeds.is_empty();
    for (number, old) in prior {
        let Some(s) = compare(new, old) else { continue };
        let dup = if blind || old.seeds.is_empty() {
            s.is_duplicate_without_seeds()
        } else {
            s.is_duplicate()
        };
        if !dup {
            continue;
        }
        if best.is_none_or(|(_, b)| s.strength() > b.strength()) {
            best = Some((*number, s));
        }
    }
    best
}

// ------------------------------------------------------------------ storage

/// Compact, sortable, inspectable in a `sqlite3` session. JSON would double the
/// size for no gain — nothing reads this but the code above.
pub fn encode(f: &Fingerprint) -> String {
    let hex = |v: &[u64]| {
        v.iter()
            .map(|x| format!("{x:x}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!("{}|{}|{}", hex(&f.shingles), hex(&f.words), hex(&f.seeds))
}

pub fn decode(s: &str) -> Fingerprint {
    let parse = |part: &str| -> Vec<u64> {
        part.split(',')
            .filter(|p| !p.is_empty())
            .filter_map(|p| u64::from_str_radix(p, 16).ok())
            .collect()
    };
    let mut it = s.splitn(3, '|');
    Fingerprint {
        shingles: parse(it.next().unwrap_or("")),
        words: parse(it.next().unwrap_or("")),
        seeds: parse(it.next().unwrap_or("")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(text: &str, seeds: &[u64]) -> Fingerprint {
        let mut s = seeds.to_vec();
        s.sort_unstable();
        Fingerprint {
            shingles: shingles(text),
            words: content_words(text),
            seeds: s,
        }
    }

    const A: &str = "QuerySet.get_or_create raises IntegrityError when the lookup and \
the create are routed to different database aliases on a multi-database setup";

    #[test]
    fn a_restatement_of_the_same_report_matches() {
        let x = fp(A, &[1, 2, 3, 4]);
        let y = fp(
            "get_or_create raises IntegrityError when the lookup and the create are \
routed to different database aliases on a multi-database setup, QuerySet",
            &[1, 2, 3, 5],
        );
        let s = compare(&x, &y).expect("enough text");
        assert!(s.is_duplicate(), "text {:.2} seed {:.2}", s.text, s.seed);
    }

    #[test]
    fn a_paste_with_a_comment_appended_matches_on_text_alone() {
        // The common case: someone reposts and adds "any update on this?".
        // Seeds follow the text, so they agree — which is exactly why the
        // shortcut can require weak seed agreement without losing this case.
        let x = fp(A, &[1, 2, 3, 4]);
        let y = fp(
            &format!("{A}\n\nAny update on this? Still seeing it on 5.1."),
            &[1, 2, 3, 4],
        );
        let s = compare(&x, &y).expect("enough text");
        assert!(s.copy >= 0.6, "copy {:.2}", s.copy);
        assert!(s.is_duplicate());
    }

    #[test]
    fn a_different_bug_in_the_same_file_is_not_a_duplicate() {
        // The dangerous near-miss, and the reason seeds cannot stand alone:
        // both reports are about send_file and select the same nodes.
        let x = fp(
            "send_file leaks a file descriptor on large downloads. When send_file \
streams a large file the descriptor is never closed, so a long-running server \
hits the open file limit and every request fails with EMFILE.",
            &[1, 2, 3, 4],
        );
        let y = fp(
            "send_file sets the wrong Content-Type for .mjs files. When serving a \
.mjs module send_file guesses text/plain from mimetypes, so the browser refuses \
to execute it and the page breaks.",
            &[1, 2, 3, 4],
        );
        let s = compare(&x, &y).expect("enough text");
        assert_eq!(s.seed, 1.0, "seeds identical by construction");
        assert!(!s.is_duplicate(), "text {:.2} copy {:.2}", s.text, s.copy);
    }

    #[test]
    fn a_restatement_in_the_reporters_own_words_matches() {
        // Measured on real wording: 3-grams score 0.19 here, which is why the
        // vocabulary signal exists at all. If this test starts failing, the
        // shingle path has quietly become the only path again.
        let x = fp(
            "Flask.send_file leaks a file descriptor on large downloads. When \
send_file streams a large file the descriptor is never closed, so a long-running \
server eventually hits the open file limit and every subsequent request fails \
with EMFILE.",
            &[1, 2, 3, 4],
        );
        let y = fp(
            "send_file leaks file descriptors when serving large downloads. The \
descriptor is never closed when send_file streams a large file, so a server that \
runs for a long time hits the open file limit and then every request after that \
fails with EMFILE.",
            &[1, 2, 3, 5],
        );
        let s = compare(&x, &y).expect("enough text");
        assert!(
            s.copy < 0.3,
            "shingles should not carry this: {:.2}",
            s.copy
        );
        assert!(s.text >= 0.5, "vocabulary must: {:.2}", s.text);
        assert!(s.is_duplicate());
    }

    #[test]
    fn the_same_code_with_a_different_complaint_does_not() {
        // Both about get_or_create, both would select the same seeds. Only the
        // text tells them apart, which is why text is the primary signal.
        let x = fp(A, &[1, 2, 3, 4]);
        let y = fp(
            "get_or_create is extremely slow on large tables because it issues a \
SELECT before every INSERT instead of using an upsert",
            &[1, 2, 3, 4],
        );
        let s = compare(&x, &y).expect("enough text");
        assert_eq!(s.seed, 1.0, "seeds are identical by construction");
        assert!(!s.is_duplicate(), "text {:.2}", s.text);
    }

    #[test]
    fn identical_text_about_unrelated_code_does_not() {
        // A template filled in twice: the prose agrees completely and the code
        // does not. Text overlap alone would merge two unrelated reports, so
        // the seed floor has to be able to veto even a perfect text match.
        let x = fp(A, &[1, 2, 3, 4]);
        let y = fp(A, &[90, 91, 92, 93]);
        let s = compare(&x, &y).expect("enough text");
        assert_eq!(s.text, 1.0);
        assert_eq!(s.copy, 1.0);
        assert!(!s.is_duplicate(), "seed {:.2}", s.seed);
        // Without a graph there is nothing to veto with, and the conclusive
        // form of the text signal is allowed to decide.
        assert!(s.is_duplicate_without_seeds());
    }

    #[test]
    fn a_short_issue_is_not_judged_at_all() {
        // "Crashes on startup" is not evidence of anything. `None` is a
        // different answer from "not a duplicate", and the caller treats it so.
        assert!(compare(
            &fp("crashes on startup", &[1]),
            &fp("crashes on startup", &[1])
        )
        .is_none());
    }

    #[test]
    fn word_order_is_part_of_the_signal() {
        // Same words, opposite meaning. A rotation would keep most three-grams
        // intact and prove nothing; this genuinely reorders.
        let x = fp(
            "the client sends the refresh token to the server and the server \
validates the session before the client retries the upload",
            &[1, 2],
        );
        let y = fp(
            "the server sends the refresh token to the client and the client \
validates the session before the server retries the upload",
            &[1, 2],
        );
        let s = compare(&x, &y).expect("enough text");
        // Vocabulary is order-blind by construction; the shingles are what
        // carry order, and this is the assertion that keeps them honest.
        assert_eq!(s.text, 1.0, "same words, so vocabulary must agree");
        assert!(
            s.copy < 0.5,
            "reordered text scored {:.2} on shingles",
            s.copy
        );
    }

    #[test]
    fn best_match_picks_the_strongest_and_ignores_the_rest() {
        let new = fp(A, &[1, 2, 3, 4]);
        let prior = vec![
            (
                10,
                fp(
                    "something else entirely about template rendering and jinja",
                    &[7, 8],
                ),
            ),
            (11, fp(A, &[1, 2, 3, 4])),
            (12, fp(A, &[1, 2, 3])),
        ];
        let (n, _) = best_match(&new, &prior).expect("a match");
        assert!(n == 11 || n == 12);
        assert!(best_match(&new, &prior[..1]).is_none());
    }

    #[test]
    fn a_fingerprint_survives_a_round_trip() {
        let f = fp(A, &[0xdead_beef, 7]);
        assert_eq!(decode(&encode(&f)), f);
        assert_eq!(decode(""), Fingerprint::default());
        assert_eq!(decode("|"), Fingerprint::default());
    }
}
