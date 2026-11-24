//! Quest browser projections. Player accounts are narrated Lore entries
//! explicitly linked to a quest, never a rendering of private quest fields.
use std::fmt::Write;

use super::{Category, Mode, about, browser::Page, entry, report};
use crate::{
    pressure, quest, time,
    world::{StoreError, World},
};

struct Account {
    id: i64,
    name: String,
    status: quest::Status,
    page: Page,
}

fn accounts(world: &World, mode: Mode) -> Result<Vec<Account>, StoreError> {
    let mut all = Vec::new();
    for q in quest::quests(world)? {
        let (name, mut page) = match mode {
            Mode::Player => {
                let Some((id, _)) = about(world, Category::Lore, q.id)? else {
                    continue;
                };
                let Some(e) = entry(world, id)? else { continue };
                let mut page = Page {
                    text: format!(
                        "{} · Player mode · {}\n\nLearned hook, goal and developments\n{}",
                        e.name,
                        q.status.as_str(),
                        report::detail(world, &e)?
                    ),
                    ..Page::default()
                };
                match q.status {
                    quest::Status::Complete => page.text.push_str("\n\nOutcome: Quest complete.\n"),
                    quest::Status::Failed => page.text.push_str("\n\nOutcome: Quest failed.\n"),
                    _ => {}
                }
                page.text.push_str("\nRelated known entries\n");
                let mut related: Vec<_> = e
                    .facts
                    .iter()
                    .flat_map(|f| f.related.iter().copied().chain(f.claimant))
                    .collect();
                related.sort_unstable();
                related.dedup();
                for id in related {
                    if id != e.id
                        && let Some(k) = entry(world, id)?
                    {
                        page.link_to(
                            &format!("#{} {}", k.id, k.name),
                            ":compendium",
                            format!("#{}", k.id),
                        );
                    }
                }
                (e.name, page)
            }
            Mode::World => {
                let mut page = Page {
                    text: format!(
                        "{} · World mode · {}\n\nHook: {}\n",
                        q.title,
                        q.status.as_str(),
                        q.hook
                    ),
                    ..Page::default()
                };
                if let Some(goal) = &q.goal {
                    writeln!(page.text, "Goal: {goal}").unwrap();
                }
                for d in &q.developments {
                    writeln!(page.text, "Development: {d}").unwrap();
                }
                if let Some(reason) = &q.resolution {
                    writeln!(page.text, "Judge resolution: {reason}").unwrap();
                }
                let now = time::now(world)?;
                for p in pressure::pressures(world)?
                    .iter()
                    .filter(|p| p.concerns.contains(&q.id))
                {
                    writeln!(
                        page.text,
                        "\nPressure #{}: {} [{}]\nStarted {}; {} · {}\nConsequence: {}",
                        p.id,
                        p.what,
                        p.status.as_str(),
                        p.started,
                        p.due_text(now),
                        p.urgency(now).as_str(),
                        p.consequence
                    )
                    .unwrap();
                    for a in &p.adjustments {
                        writeln!(
                            page.text,
                            "  At {}: {:+} min · {}",
                            time::Moment(a.at),
                            a.minutes,
                            a.reason
                        )
                        .unwrap();
                    }
                    if let Some(r) = &p.resolution {
                        writeln!(page.text, "Resolution: {r}").unwrap();
                    }
                }
                page.text.push_str("\nConcerned entities\n");
                let world_entries = super::world_entries(world)?;
                for &id in &q.concerns {
                    if let Some(e) = world.entity(id)? {
                        let key = format!("#{id}");
                        let label = format!("{key} {}", e.name);
                        if world_entries.iter().any(|e| e.key == key) {
                            page.link_to(&label, ":compendium", key);
                        } else {
                            writeln!(page.text, "{label}").unwrap();
                        }
                    }
                }
                (q.title, page)
            }
        };
        page.key = Some(format!("#{}", q.id));
        all.push(Account {
            id: q.id,
            name,
            status: q.status,
            page,
        });
    }
    Ok(all)
}

/// `:quests [ongoing|history|<name|#quest>|search <text>]` in the
/// already-resolved shared mode. Filtering precedes matching and counting.
pub fn query(world: &World, mode: Mode, arg: &str) -> Result<Page, StoreError> {
    let all = accounts(world, mode)?;
    let arg = arg.trim();
    let ongoing = arg.is_empty() || arg.eq_ignore_ascii_case("ongoing");
    let history = arg.eq_ignore_ascii_case("history");
    let search = arg
        .split_once(char::is_whitespace)
        .filter(|(w, _)| w.eq_ignore_ascii_case("search"))
        .map(|(_, text)| text.trim().to_lowercase());
    let mut hits: Vec<_> = if ongoing || history {
        all.iter()
            .filter(|a| a.status.is_resolved() == history)
            .collect()
    } else if let Some(text) = &search {
        all.iter()
            .filter(|a| a.page.text.to_lowercase().contains(text))
            .collect()
    } else if let Some(id) = arg.strip_prefix('#').and_then(|s| s.parse::<i64>().ok()) {
        all.iter().filter(|a| a.id == id).collect()
    } else {
        let exact: Vec<_> = all
            .iter()
            .filter(|a| a.name.eq_ignore_ascii_case(arg))
            .collect();
        if exact.is_empty() {
            all.iter()
                .filter(|a| a.name.to_lowercase().contains(&arg.to_lowercase()))
                .collect()
        } else {
            exact
        }
    };
    if !ongoing && !history && search.is_none() && hits.len() == 1 {
        return Ok(hits.remove(0).page.clone());
    }
    let heading = if ongoing {
        "Ongoing quests"
    } else if history {
        "Quest history"
    } else {
        "Quest matches"
    };
    let mut page = Page {
        text: format!("{heading} · {} mode\n\n", mode.label()),
        ..Page::default()
    };
    if hits.is_empty() {
        page.text.push_str("No quests found.\n");
    }
    for a in hits {
        page.link_to(
            &format!("#{} {} [{}]", a.id, a.name, a.status.as_str()),
            ":quests",
            format!("#{}", a.id),
        );
    }
    page.text.push('\n');
    page.link_to("Browse ongoing quests", ":quests", "ongoing".into());
    page.link_to("Browse quest history", ":quests", "history".into());
    Ok(page)
}
