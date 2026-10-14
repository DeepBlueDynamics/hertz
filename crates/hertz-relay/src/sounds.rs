//! "Sounds like": find a phrase in a call's opening words that sounds like a whole
//! name the speaker could be calling ("Stables arena" for "Stable Xerinae").
//!
//! ollaya reads text, not sound, so it can't tell that a misheard name rhymes with a
//! pane name. This finds the likely match and ollaya is told about it; ollaya still
//! makes the decision. Because ollaya trusts such a note almost completely (a wrong
//! "stable day sounds like Stable Xerinae" note routed a non-call at 1.00), matching
//! is strict: the whole name, inside the call-up, with a close phonetic match.
//! On 15 misheard real call-ups and 16 non-calls it found 13 with the right name,
//! never a wrong one, and nothing on the non-calls.

/// Only the call-up is searched: the name comes first on the radio.
const CALLUP_WORDS: usize = 8;
/// Minimum phonetic similarity (1 - edit distance / length of the longer key).
const MIN_SIMILARITY: f64 = 0.75;

/// A phrase in the call-up that sounds like `name`.
#[derive(Clone, Debug, PartialEq)]
pub struct SoundsLike {
    pub phrase: String,
    pub name: String,
    pub similarity: f64,
}

/// The best sounds-like match of any of `names` in the first words of `transcript`,
/// skipping phrases that already spell the name exactly (no note needed).
pub fn sounds_like(transcript: &str, names: &[String]) -> Option<SoundsLike> {
    let words: Vec<&str> = transcript
        .split(|c: char| c.is_whitespace() || ",.!?;:".contains(c))
        .filter(|w| !w.is_empty())
        .take(CALLUP_WORDS)
        .collect();
    let mut best: Option<SoundsLike> = None;
    for name in names {
        let name_words: Vec<&str> = name.split_whitespace().collect();
        let name_key: String = name_words.iter().map(|w| key(w)).collect();
        if name_key.is_empty() {
            continue;
        }
        let n = name_words.len();
        // Spelled correctly already: ollaya reads it fine, no note needed.
        if words.windows(n).any(|w| {
            w.iter()
                .zip(&name_words)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        }) {
            continue;
        }
        for len in [n.saturating_sub(1), n, n + 1] {
            if len == 0 {
                continue;
            }
            for phrase in words.windows(len) {
                let phrase_text = phrase.join(" ");
                if phrase_text.eq_ignore_ascii_case(name) {
                    continue;
                }
                let phrase_key: String = phrase.iter().map(|w| key(w)).collect();
                let similarity = similarity(&phrase_key, &name_key);
                if similarity >= MIN_SIMILARITY
                    && best.as_ref().is_none_or(|b| similarity > b.similarity)
                {
                    best = Some(SoundsLike {
                        phrase: phrase_text,
                        name: name.clone(),
                        similarity,
                    });
                }
            }
        }
    }
    best
}

/// A compact metaphone-style key: consonant skeleton with common English spellings
/// of the same sound folded together; a leading vowel is kept as 'A'.
fn key(word: &str) -> String {
    let w: String = word
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if w.is_empty() {
        return String::new();
    }
    let mut s = w;
    if let Some(rest) = s.strip_prefix('x') {
        s = format!("s{rest}");
    }
    for p in ["kn", "gn", "pn", "wr"] {
        if s.starts_with(p) {
            s = s[1..].to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("wh") {
        s = format!("w{rest}");
    }
    for (from, to) in [
        ("ph", "f"),
        ("ck", "k"),
        ("sch", "sk"),
        ("sh", "x"),
        ("ch", "x"),
        ("th", "0"),
    ] {
        s = s.replace(from, to);
    }
    // Context-dependent letters.
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let soft = matches!(next, Some('i' | 'e' | 'y'));
        match c {
            'c' if soft => out.push('s'),
            'c' | 'q' => out.push('k'),
            'x' => out.push_str("ks"),
            'z' => out.push('s'),
            'v' => out.push('f'),
            'd' if next == Some('g') => {
                out.push('j');
                i += 1;
            }
            'g' if next == Some('h') => i += 1,
            'g' if soft => out.push('j'),
            _ => out.push(c),
        }
        i += 1;
    }
    // Collapse doubled letters.
    let mut dedup = String::new();
    for c in out.chars() {
        if !dedup.ends_with(c) {
            dedup.push(c);
        }
    }
    let mut it = dedup.chars();
    let first = it.next().unwrap_or_default();
    let rest: String = it.filter(|c| !"aeiouyhw".contains(*c)).collect();
    let head = if "aeiou".contains(first) { 'A' } else { first };
    format!("{head}{rest}")
}

fn similarity(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let longest = a.len().max(b.len()).max(1);
    1.0 - levenshtein(&a, &b) as f64 / longest as f64
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j + 1] + 1)
                .min(cur[j] + 1)
                .min(prev[j] + usize::from(ca != cb));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        [
            "Circular Slug",
            "Fond Egret",
            "Pale Frog",
            "Stable Xerinae",
            "Magnificent Vole",
            "Oscar Kilo",
            "Papa Golf",
            "Annual Echidna",
            "Brief Mite",
            "Burning Gerbil",
            "Quebec Lima",
            "Top Rabbit",
            "Repulsive Squirrel",
            "Industrial Pike",
        ]
        .map(String::from)
        .to_vec()
    }

    /// Real misheard call-ups from the hertzd logs.
    #[test]
    fn finds_misheard_callups() {
        for (heard, want) in [
            (
                "Stables arena, stables arena theoretically.",
                "Stable Xerinae",
            ),
            (
                "Zarina stable zarina after Pilo. How about you tell them",
                "Stable Xerinae",
            ),
            ("Fund egret, fund egret, come in.", "Fond Egret"),
            ("Kubeck, Thema, Kubeck, Thema, come in over.", "Quebec Lima"),
            ("K-back Lima, K-back Lima, command over.", "Quebec Lima"),
            (
                "Industrial bike, industrial bike. This is alpha tango",
                "Industrial Pike",
            ),
            (
                "Pulsive squirrel, pulsive squirrel. Can you make it",
                "Repulsive Squirrel",
            ),
            ("Might, brief might, come in over.", "Brief Mite"),
            (
                "Tough rabbit, tough rabbit, you're doing good work.",
                "Top Rabbit",
            ),
        ] {
            let hit = sounds_like(heard, &names());
            assert_eq!(
                hit.as_ref().map(|h| h.name.as_str()),
                Some(want),
                "{heard:?} -> {hit:?}"
            );
        }
    }

    /// Real non-calls: no note, or ollaya would be told a name was called.
    #[test]
    fn ignores_non_calls() {
        for heard in [
            "Thank you, have a stable day.",
            "Bye.",
            "Thank you.",
            "Alpha India, this is Alpha Tango, come in over.",
            "Keep that clean, okay, back clean, come in over.",
            "No, I kidnapped, come in over, anyone kidnapped him back.",
            "This is the angle coming over.",
            "The pale moon in the pond, over.",
            "Golf course is open today",
        ] {
            assert_eq!(sounds_like(heard, &names()), None, "{heard:?}");
        }
    }

    #[test]
    fn exact_name_needs_no_note() {
        assert_eq!(
            sounds_like("Fond Egret, come in.", &["Fond Egret".into()]),
            None
        );
    }
}
