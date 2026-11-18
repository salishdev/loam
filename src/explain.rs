//! First-time notes and `:explain`: a sentence or two on each workbench
//! concept, shown the first time it turns up in a save.
//!
//! A note is due when its concept appears while X-ray is on
//! ([`turn_topics`]) or when a workbench view first involves it (the front
//! ends name the topic). [`Notes`] keeps which topics this save has
//! already explained: read from meta `explained`, written back through
//! the worker (the front ends' view of the save is read-only). Nothing
//! here calls a model.

use std::collections::BTreeSet;

use crate::reroll::Summary;
use crate::world::{StoreError, World};
use crate::xray::{Path, TurnXray};

/// A concept the workbench explains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Topic {
    Cache,
    FastPath,
    Director,
    Refusal,
    Rule,
    RerollSplit,
    Context,
    Brain,
    Branch,
    Cost,
}

impl Topic {
    /// Every topic, in the order `:explain` lists them.
    pub const ALL: [Topic; 10] = [
        Topic::Director,
        Topic::FastPath,
        Topic::Rule,
        Topic::Refusal,
        Topic::Context,
        Topic::Cache,
        Topic::Cost,
        Topic::Brain,
        Topic::Branch,
        Topic::RerollSplit,
    ];

    /// The name `:explain` takes and meta `explained` stores.
    pub fn name(self) -> &'static str {
        match self {
            Topic::Cache => "cache",
            Topic::FastPath => "fast-path",
            Topic::Director => "director",
            Topic::Refusal => "refusal",
            Topic::Rule => "rule",
            Topic::RerollSplit => "reroll-split",
            Topic::Context => "context",
            Topic::Brain => "brain",
            Topic::Branch => "branch",
            Topic::Cost => "cost",
        }
    }

    /// The explanation: one or two plain sentences.
    pub fn text(self) -> &'static str {
        match self {
            Topic::Cache => {
                "Every call resends long instructions the model has seen moments before; \
                 the API can reuse that cached start of the prompt for a tenth of the price \
                 and answer sooner. \"Cached\" is how much of the prompt was reused."
            }
            Topic::FastPath => {
                "A small, cheap model recognised your action as one a rule already covers, \
                 so the engine applied the rule and only the narration was written fresh. \
                 No director call: quicker, cheaper, and consistent with last time."
            }
            Topic::Director => {
                "Nothing on the books covered what you did, so the director, the large \
                 model, ruled on it with tools: changing the world, rolling dice, and \
                 sometimes writing a rule so the next time is cheap."
            }
            Topic::Refusal => {
                "The model asked for something the engine's rules don't allow, so the \
                 engine said no. Models can be talked into things; the engine can't."
            }
            Topic::Rule => {
                "The director wrote its ruling down as a rule: a trigger, conditions and \
                 effects. When the same kind of action comes up again, the engine applies \
                 the rule instead of asking a model to decide afresh."
            }
            Topic::RerollSplit => {
                "The same moment, replayed from the same world, came out differently: \
                 that spread is the model's own variability. The summary counts how many \
                 variants took each outcome."
            }
            Topic::Context => {
                "A call's context is everything the model was sent: instructions, the \
                 slice of the world it was shown, earlier messages and tool results. A \
                 model knows only what's in its context, so when it forgets something, \
                 look here first."
            }
            Topic::Brain => {
                "Each role (director, narrator, interpreter, judge, tick) runs on a model \
                 at an effort level. Bigger models and more effort usually rule better but \
                 cost more and answer slower; changes last for this session."
            }
            Topic::Branch => {
                "A branch is a separate save that shares this game's history up to the \
                 event it was forked at. Playing one never changes the others; :branches \
                 shows them and :switch moves between them."
            }
            Topic::Cost => {
                "Calls are billed per token: what's sent, what comes back, and cached \
                 input at a discount. The costs shown are estimates from loam's price \
                 table, not your bill."
            }
        }
    }

    /// A few words for the `:explain` list.
    pub fn summary(self) -> &'static str {
        match self {
            Topic::Director => "the large model ruling on something new",
            Topic::FastPath => "a rule applied without the director",
            Topic::Rule => "a ruling written down for next time",
            Topic::Refusal => "the engine saying no to a model",
            Topic::Context => "everything a model was sent for one call",
            Topic::Cache => "reused prompts: cheaper and quicker",
            Topic::Cost => "what the calls are estimated to cost",
            Topic::Brain => "the model and effort each role uses",
            Topic::Branch => "a separate save sharing this game's past",
            Topic::RerollSplit => "the same moment coming out differently",
        }
    }

    /// The topic named `name` (case and surrounding space ignored).
    pub fn parse(name: &str) -> Option<Topic> {
        let name = name.trim().to_lowercase();
        Topic::ALL.into_iter().find(|t| t.name() == name)
    }

    /// A note's one line: `topic: text`.
    pub fn line(self) -> String {
        format!("{}: {}", self.name(), self.text())
    }
}

/// `:explain` (every topic, a line each) or `:explain <topic>`; an unknown
/// topic says so and lists the topics.
pub fn explain(arg: &str) -> String {
    let arg = arg.trim();
    if arg.is_empty() {
        return list();
    }
    match Topic::parse(arg) {
        Some(t) => t.line(),
        None => format!("No topic {arg}.\n\n{}", list()),
    }
}

/// Every topic with a few words on it.
fn list() -> String {
    let mut out = String::from("Topics (:explain <topic> for one):");
    for t in Topic::ALL {
        out.push_str(&format!("\n  {:<13} {}", t.name(), t.summary()));
    }
    out
}

/// The concepts a turn's X-ray shows: how it was resolved, cache use,
/// cost, refusals and rule changes.
pub fn turn_topics(x: &TurnXray) -> Vec<Topic> {
    let mut topics = vec![match x.path {
        Path::Director => Topic::Director,
        Path::FastPath(_) => Topic::FastPath,
    }];
    if x.cached_tokens() > 0 {
        topics.push(Topic::Cache);
    }
    if x.cost_usd > 0.0 {
        topics.push(Topic::Cost);
    }
    if !x.refusals.is_empty() {
        topics.push(Topic::Refusal);
    }
    if !x.rules.is_empty() {
        topics.push(Topic::Rule);
    }
    topics
}

/// The concepts a finished reroll shows: a split, when the variants
/// disagreed on their route or what changed, and its cost.
pub fn reroll_topics(s: &Summary) -> Vec<Topic> {
    let succeeded = s.done - s.failed.len();
    let split = s.routes.len() > 1 || s.facts.iter().any(|(_, ns)| ns.len() < succeeded);
    let mut topics = Vec::new();
    if split {
        topics.push(Topic::RerollSplit);
    }
    if s.cost_usd > 0.0 {
        topics.push(Topic::Cost);
    }
    topics
}

/// Which topics this save has explained, as one session sees them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Notes {
    shown: BTreeSet<Topic>,
}

impl Notes {
    /// The topics `world` has already explained (meta `explained`).
    pub fn load(world: &World) -> Notes {
        Notes {
            shown: world.explained().unwrap_or_default(),
        }
    }

    /// Every topic already shown, for tests about something else.
    #[cfg(test)]
    pub(crate) fn all() -> Notes {
        Notes {
            shown: Topic::ALL.into_iter().collect(),
        }
    }

    /// The topics of `topics` not shown yet, in order and without repeats,
    /// now counted as shown. The caller shows them and asks the worker to
    /// save them ([`crate::worker::Command::Explained`]).
    pub fn fresh(&mut self, topics: impl IntoIterator<Item = Topic>) -> Vec<Topic> {
        topics
            .into_iter()
            .filter(|t| self.shown.insert(*t))
            .collect()
    }
}

impl World {
    /// The topics this save has explained (meta `explained`, a JSON array
    /// of names; unknown names are ignored).
    pub fn explained(&self) -> Result<BTreeSet<Topic>, StoreError> {
        let Some(v) = self.meta("explained")? else {
            return Ok(BTreeSet::new());
        };
        let names: Vec<String> = serde_json::from_str(&v)
            .map_err(|_| StoreError::Corrupt(format!("meta explained = {v:?}")))?;
        Ok(names.iter().filter_map(|n| Topic::parse(n)).collect())
    }

    /// Record `topics` as explained in this save.
    pub fn mark_explained(&self, topics: &[Topic]) -> Result<(), StoreError> {
        let mut all = self.explained()?;
        if topics.iter().all(|t| all.contains(t)) {
            return Ok(());
        }
        all.extend(topics.iter().copied());
        let names: Vec<&str> = all.iter().map(|t| t.name()).collect();
        self.set_meta("explained", &serde_json::to_string(&names).expect("names"))
    }
}

#[cfg(test)]
mod tests;
