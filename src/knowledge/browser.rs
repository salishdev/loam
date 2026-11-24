//! Selectable compendium queries. Both front ends use the same projections
//! and detail reports; links are explicit targets, never parsed display text.
use super::{Category, Entry, Found, Mode, WorldFound, compendium_in, entries, find, search};
use super::{report, truth, world_entries, world_find, world_search};
use crate::world::{StoreError, World};

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// Unwrapped text line to highlight.
    pub line: usize,
    /// An exact compendium argument, interpreted in this view's mode.
    pub arg: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Page {
    /// Exact identity after resolving an entry, stable across renames.
    pub key: Option<String>,
    pub text: String,
    pub links: Vec<Link>,
}

impl Page {
    fn link(&mut self, label: &str, arg: String) {
        self.links.push(Link {
            line: self.text.lines().count(),
            arg,
        });
        self.text.push_str(label);
        self.text.push('\n');
    }
}

/// Source events mentioned by an entry's remembered account, never a raw
/// entity's events or a tick's hidden narration.
pub(super) fn sources(e: &Entry) -> Vec<i64> {
    let mut ids = vec![e.first_seen, e.last_seen, e.summary_source];
    ids.extend(
        e.facts
            .iter()
            .flat_map(|f| f.sources.iter().map(|s| s.event)),
    );
    if let Some(u) = e.usage {
        ids.extend([u.first, u.last]);
    }
    ids.retain(|n| *n > 0);
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// A category menu, a selectable list, or one detailed account. Every
/// label, count, match and link is made from the selected projection.
pub fn query(world: &World, mode: Mode, arg: &str) -> Result<Page, StoreError> {
    let arg = arg.trim();
    if arg.is_empty() {
        let categories: Vec<Category> = match mode {
            Mode::Player => entries(world)?.iter().map(|e| e.category).collect(),
            Mode::World => world_entries(world)?.iter().map(|e| e.category).collect(),
        };
        let mut page = Page {
            text: format!("Compendium · {} mode\n\n", mode.label()),
            ..Page::default()
        };
        for c in Category::ALL {
            let count = categories.iter().filter(|&&of| of == c).count();
            page.link(
                &format!("{} ({count})", c.heading()),
                c.heading().to_lowercase(),
            );
        }
        return Ok(page);
    }
    if arg.starts_with("source ") {
        return Ok(Page {
            text: compendium_in(world, mode, arg)?,
            ..Page::default()
        });
    }
    let category = Category::parse(arg);
    let search_text = arg
        .split_once(char::is_whitespace)
        .filter(|(word, _)| word.eq_ignore_ascii_case("search"))
        .map(|(_, rest)| rest.trim());
    let heading = category
        .map(|c| c.heading().to_string())
        .unwrap_or_else(|| format!("Search: {}", search_text.unwrap_or(arg)));
    let mut page = Page {
        text: format!("{heading} · {} mode\n\n", mode.label()),
        ..Page::default()
    };
    match mode {
        Mode::Player => {
            let found = if category.is_some() {
                entries(world)?
            } else if let Some(text) = search_text {
                search(world, text)?
            } else {
                match find(world, arg)? {
                    Found::One(e) => return player_detail(world, &e),
                    Found::Many(es) => es,
                    Found::None => Vec::new(),
                }
            };
            for e in found
                .into_iter()
                .filter(|e| category.is_none_or(|c| e.category == c))
            {
                page.link(
                    &format!("#{} {} ({})", e.id, e.name, e.category.label()),
                    format!("#{}", e.id),
                );
            }
        }
        Mode::World => {
            let found = if category.is_some() {
                world_entries(world)?
            } else if let Some(text) = search_text {
                world_search(world, text)?
            } else {
                match world_find(world, arg)? {
                    WorldFound::One(e) => return world_detail(world, &e),
                    WorldFound::Many(es) => es,
                    WorldFound::None => Vec::new(),
                }
            };
            for e in found
                .into_iter()
                .filter(|e| category.is_none_or(|c| e.category == c))
            {
                page.link(
                    &format!(
                        "{} {} ({} · {})",
                        e.key,
                        e.name,
                        e.category.label(),
                        e.origin.label()
                    ),
                    e.key,
                );
            }
        }
    }
    if page.links.is_empty() {
        page.text.push_str("No entries found.\n");
    }
    Ok(page)
}

fn player_detail(world: &World, e: &Entry) -> Result<Page, StoreError> {
    let mut page = Page {
        key: Some(format!("#{}", e.id)),
        text: report::detail(world, e)?,
        ..Page::default()
    };
    page.text.push_str("\n\nRelated entries\n");
    let mut related: Vec<_> = e.facts.iter().filter_map(|f| f.claimant).collect();
    related.extend(e.seen_at);
    related.sort_unstable();
    related.dedup();
    // Resolve only against learned entries; never dereference their world links.
    for id in related {
        if id != e.id
            && let Some(known) = super::entry(world, id)?
        {
            page.link(
                &format!("#{} {}", known.id, known.name),
                format!("#{}", known.id),
            );
        }
    }
    page.text.push_str("\nSources · Enter reads narration\n");
    for id in sources(e) {
        page.link(&format!("Narration #{id}"), format!("source #{id}"));
    }
    Ok(page)
}

fn world_detail(world: &World, e: &super::WorldEntry) -> Result<Page, StoreError> {
    let mut page = Page {
        key: Some(e.key.clone()),
        text: truth::world_detail(e),
        ..Page::default()
    };
    // World links use current containment. Bible-only subjects have no entity.
    if let Some(id) = e.key.strip_prefix('#').and_then(|id| id.parse().ok())
        && let Some(entity) = world.entity(id)?
    {
        let mut related = world.contents(Some(id))?;
        related.extend(entity.location);
        let all = world_entries(world)?;
        page.text.push_str("\n\nRelated entries\n");
        for known in all
            .into_iter()
            .filter(|k| related.iter().any(|id| k.key == format!("#{id}")))
        {
            page.link(&format!("{} {}", known.key, known.name), known.key);
        }
    }
    Ok(page)
}
