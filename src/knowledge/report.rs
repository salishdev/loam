//! `:compendium`: the Player-mode compendium as a text report, shared by
//! plain mode and the TUI's report panel. Reads entries only: no model
//! call, no write.

use std::fmt::Write;

use super::{Category, Entry, FactKind, Found, entries, entry, find, search};
use crate::world::{StoreError, World};

const USAGE: &str = "Usage: :compendium [people | places | factions | items | lore | mechanics | <name|#entry> | search <text>]";
const HINT: &str = ":compendium <name> shows an entry; :compendium search <text> searches.";

/// `:compendium [<category> | <name|#entry> | search <text>]`.
pub fn compendium(world: &World, arg: &str) -> Result<String, StoreError> {
    let arg = arg.trim();
    let (word, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
    if arg.is_empty() {
        return overview(world);
    }
    if word.eq_ignore_ascii_case("search") {
        return find_text(world, rest.trim());
    }
    if rest.is_empty()
        && let Some(category) = Category::parse(word)
    {
        return section(world, category);
    }
    Ok(match find(world, arg)? {
        Found::One(e) => detail(world, &e)?,
        Found::None => format!("You know of nothing called {arg:?}.\n{USAGE}"),
        Found::Many(many) => {
            let mut out = format!("{arg:?} could be several entries; use an id:");
            for e in &many {
                write!(out, "\n  {}", line(e)).unwrap();
            }
            out
        }
    })
}

fn overview(world: &World) -> Result<String, StoreError> {
    let all = entries(world)?;
    if all.is_empty() {
        return Ok(
            "Your compendium is empty: places are remembered as you enter them, and what you \
             learn as it is narrated."
                .into(),
        );
    }
    let mut out = String::from("Compendium: what you have learned\n");
    for c in Category::ALL {
        let n = all.iter().filter(|e| e.category == c).count();
        if n > 0 {
            writeln!(out, "  {} ({n})", c.heading()).unwrap();
        }
    }
    for c in Category::ALL {
        let known: Vec<&Entry> = all.iter().filter(|e| e.category == c).collect();
        if known.is_empty() {
            continue;
        }
        writeln!(out, "\n{}", c.heading()).unwrap();
        for e in known {
            writeln!(out, "  {}", line(e)).unwrap();
        }
    }
    write!(out, "\n{HINT}").unwrap();
    Ok(out)
}

fn section(world: &World, category: Category) -> Result<String, StoreError> {
    let known: Vec<Entry> = entries(world)?
        .into_iter()
        .filter(|e| e.category == category)
        .collect();
    if known.is_empty() {
        return Ok(format!(
            "No {} known yet.",
            category.heading().to_lowercase()
        ));
    }
    let mut out = format!("{}\n", category.heading());
    for e in &known {
        writeln!(out, "  {}", line(e)).unwrap();
    }
    write!(out, "\n{HINT}").unwrap();
    Ok(out)
}

fn find_text(world: &World, text: &str) -> Result<String, StoreError> {
    if text.is_empty() {
        return Ok(USAGE.into());
    }
    let hits = search(world, text)?;
    if hits.is_empty() {
        return Ok(format!("Nothing you know mentions {text:?}."));
    }
    let mut out = format!("Entries mentioning {text:?}:");
    for e in &hits {
        write!(out, "\n  {}", line(e)).unwrap();
    }
    Ok(out)
}

/// `#12 Drowned Chapel (place)`.
fn line(e: &Entry) -> String {
    format!("#{} {} ({})", e.id, e.name, e.category.label())
}

/// One entry in full, with the events that support it.
fn detail(world: &World, e: &Entry) -> Result<String, StoreError> {
    let mut out = format!("{} · {} · entry #{}\n", e.name, e.category.label(), e.id);
    let has_claims = e.facts.iter().any(|f| f.kind == FactKind::Claimed);
    if e.category == Category::Place && e.encounters > 0 {
        if e.summary.is_empty() {
            writeln!(out, "(nothing described) [#{}]", e.summary_source).unwrap();
        } else {
            writeln!(out, "{} [#{}]", e.summary, e.summary_source).unwrap();
        }
    } else if !e.summary.is_empty() {
        writeln!(out, "{}", e.summary).unwrap();
    } else if has_claims {
        writeln!(out, "Known only from what others say.").unwrap();
    }
    if let Some(place) = e.seen_at {
        // The place as the player knows it: an entry, never the room.
        if let Some(p) = entry(world, place)? {
            writeln!(out, "Last seen at {} [#{}].", p.name, e.last_seen).unwrap();
        }
    }
    if !e.facts.is_empty() {
        out.push('\n');
        let superseded = e.superseded();
        for (i, f) in e.facts.iter().enumerate() {
            let n = i + 1;
            let sources: Vec<String> = f
                .sources
                .iter()
                .map(|s| match &s.excerpt {
                    Some(x) => format!("#{} {x:?}", s.event),
                    None => format!("#{}", s.event),
                })
                .collect();
            let text = match (&f.kind, &f.claimed_by) {
                (FactKind::Claimed, Some(by)) => {
                    // The claimant's entry, when known: an entry, never a
                    // world entity, so nothing unlearned is named.
                    let known = match f.claimant {
                        Some(id) => entry(world, id)?.map(|c| format!(" (#{})", c.id)),
                        None => None,
                    };
                    format!("{by}{} claims: {}", known.unwrap_or_default(), f.text)
                }
                (FactKind::Claimed, None) => format!("It is said: {}", f.text),
                (FactKind::Observed, _) => f.text.clone(),
            };
            let mut notes = Vec::new();
            if !f.supersedes.is_empty() {
                let list: Vec<String> = f.supersedes.iter().map(|k| k.to_string()).collect();
                notes.push(format!("corrects {}", list.join(", ")));
            }
            if superseded.contains(&n) {
                let by: Vec<String> = e
                    .facts
                    .iter()
                    .enumerate()
                    .filter(|(_, g)| g.supersedes.contains(&n))
                    .map(|(j, _)| (j + 1).to_string())
                    .collect();
                notes.push(format!("superseded by {}", by.join(", ")));
            }
            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", notes.join("; "))
            };
            writeln!(out, "{n}. {text}{notes} [{}]", sources.join("; ")).unwrap();
        }
    }
    if e.encounters == 0 {
        write!(
            out,
            "\nLearned from narration #{} to #{}. [#n] is the narration it comes from: :log #n \
             shows it.",
            e.first_seen, e.last_seen
        )
        .unwrap();
        return Ok(out);
    }
    let times = if e.encounters == 1 {
        "once".to_owned()
    } else {
        format!("{} times", e.encounters)
    };
    write!(
        out,
        "\nFirst seen #{}, last seen #{} ({times}). [#n] is the narration it comes from: \
         :log #n shows it.",
        e.first_seen, e.last_seen
    )
    .unwrap();
    Ok(out)
}
