//! `:compendium`: the compendium as a text report, shared by plain mode
//! and the TUI's report panel, with `:rules` and `:glossary` as shortcuts
//! into its Mechanics section. Player mode reads learned entries only
//! (never a rule or glossary term an entry is linked to); World mode
//! ([`super::truth`]) reads stored world state and the bible. Neither calls
//! a model or writes.

use std::fmt::Write;

use super::truth::{Mode, WorldFound, split_mode, world_find, world_report};
use super::{Category, Entry, FactKind, Found, entries, entry, find, mechanics, search};
use crate::world::{StoreError, World};

pub(super) const USAGE: &str = "Usage: :compendium [player | world] [people | places | factions | items | lore | mechanics | <name|#entry> | search <text>]";
const HINT: &str = ":compendium <name> shows an entry; :compendium search <text> searches.";

/// `:compendium [player | world] [<category> | <name|#entry> | search
/// <text>]` in Player mode unless the argument names World.
pub fn compendium(world: &World, arg: &str) -> Result<String, StoreError> {
    let (mode, arg) = resolve_mode(world, Mode::Player, arg)?;
    compendium_in(world, mode, arg)
}

/// The mode a `:compendium` argument asks for, read in `mode`, and the
/// rest of it. A leading `player` or `world` names a mode before nothing,
/// a section, `search …` or a key ([`split_mode`]); before anything else
/// it does too, unless the whole argument names something in `mode` (a
/// learned "World Tree"). Read once, as typed: what is left is never
/// read for a mode again.
pub fn resolve_mode<'a>(
    world: &World,
    mode: Mode,
    arg: &'a str,
) -> Result<(Mode, &'a str), StoreError> {
    let arg = arg.trim();
    if let (Some(named), rest) = split_mode(arg) {
        return Ok((named, rest));
    }
    let Some((word, rest)) = arg.split_once(char::is_whitespace) else {
        return Ok((mode, arg));
    };
    let Some(named) = Mode::parse(word) else {
        return Ok((mode, arg));
    };
    let names_something = match mode {
        Mode::Player => find(world, arg)? != Found::None,
        Mode::World => world_find(world, arg)? != WorldFound::None,
    };
    Ok(if names_something {
        (mode, arg)
    } else {
        (named, rest.trim())
    })
}

/// `:compendium` in `mode`, with the argument taken as it is (no mode
/// word: see [`resolve_mode`]).
pub fn compendium_in(world: &World, mode: Mode, arg: &str) -> Result<String, StoreError> {
    match mode {
        Mode::Player => player(world, arg),
        Mode::World => world_report(world, arg),
    }
}

/// The Player-mode report: learned entries only.
fn player(world: &World, arg: &str) -> Result<String, StoreError> {
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
        if category == Category::Mechanic {
            return mechanics_section(world);
        }
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
    let mut out = String::from("Compendium · Player mode: what you have learned\n");
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
    let mut out = format!("{} · Player mode\n", category.heading());
    for e in &known {
        writeln!(out, "  {}", line(e)).unwrap();
    }
    write!(out, "\n{HINT}").unwrap();
    Ok(out)
}

/// The Mechanics section: interactions, then terms.
fn mechanics_section(world: &World) -> Result<String, StoreError> {
    let m = mechanics(world)?;
    if m.interactions.is_empty() && m.terms.is_empty() {
        return Ok(NO_MECHANICS.into());
    }
    let mut out = String::from("Mechanics\n");
    for (heading, known) in [("Interactions", &m.interactions), ("Terms", &m.terms)] {
        if known.is_empty() {
            continue;
        }
        writeln!(out, "  {heading}").unwrap();
        for e in known {
            writeln!(out, "    {}", line(e)).unwrap();
        }
    }
    write!(out, "\n{HINT}").unwrap();
    Ok(out)
}

const NO_MECHANICS: &str = "No mechanics known yet: how things work is remembered as you \
                            learn it in play.";

/// `:rules`: the interactions the player has learned, each in full. Never
/// the engine's rules themselves.
pub fn learned_rules(world: &World) -> Result<String, StoreError> {
    learned(
        world,
        mechanics(world)?.interactions,
        "Mechanics: interactions you have learned",
        "No interactions known yet: how things work is remembered as you learn it in play.",
    )
}

/// `:glossary`: the terminology the player has learned, each in full.
/// Never the engine's glossary itself.
pub fn learned_terms(world: &World) -> Result<String, StoreError> {
    learned(
        world,
        mechanics(world)?.terms,
        "Mechanics: terms you have learned",
        "No terms known yet: terminology is remembered as you learn it in play.",
    )
}

fn learned(
    world: &World,
    known: Vec<Entry>,
    heading: &str,
    none: &str,
) -> Result<String, StoreError> {
    if known.is_empty() {
        return Ok(none.into());
    }
    let mut out = format!("{heading}\n");
    for e in &known {
        writeln!(out, "\n{}", detail(world, e)?).unwrap();
    }
    write!(out, "\n:compendium mechanics lists every mechanic.").unwrap();
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
    if let Some(u) = e.usage {
        let times = if u.count == 1 {
            "once".to_owned()
        } else {
            format!("{} times", u.count)
        };
        writeln!(out, "Used {times}: first #{}, last #{}.", u.first, u.last).unwrap();
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
