//! Rerolls: run one turn again, several times, and compare.
//!
//! Each variant branches the world in memory just before the turn's input
//! ([`World::fork_in_memory`]) and plays the whole turn again from the raw
//! input: interpretation (which may route differently), the director or the
//! fast path, narration and the quest judge. No world tick follows. The
//! save being played gains only call records (tagged with the reroll and
//! variant) and a `rerolls` row; its log and state are untouched.
//!
//! Variant 1 runs first, so the prompt cache is warm for the rest, which
//! then run in parallel threads. What changed in each variant is computed
//! by the engine ([`Fact`]s from a state diff and the turn's events), and
//! [`summarize`] groups identical changes across variants. No model call is
//! needed to compare them.

pub mod surgery;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::mpsc::channel;

use serde_json::{Value, json};

use crate::game::{DirectorPrompt, Game, TurnOutcome};
use crate::llm::{Brains, CallRecord, Effort, LlmClient, Model};
use crate::world::{
    ENGINE_PROP_PREFIX, Entity, EntityId, EventKind, Relation, StoreError, World, WorldState,
};
use crate::xray;

/// Variants when `:reroll` doesn't say.
pub const DEFAULT_N: usize = 3;
/// Most variants one reroll may run.
pub const MAX_N: usize = 8;

/// What `:reroll` accepts.
pub const USAGE: &str = "Usage: :reroll [#turn] [n] [director=<haiku|sonnet|opus>] \
[effort=<low|medium|high|xhigh|max>] [dice=fresh] | :reroll show <n>";

/// What to reroll and how.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// The turn's input event; `None` is the latest turn.
    pub turn: Option<i64>,
    /// How many variants (1 to [`MAX_N`]).
    pub n: usize,
    /// The director's model for the variants, if not the session's.
    pub director: Option<Model>,
    /// The director's effort for the variants, if not the session's.
    pub effort: Option<Effort>,
    /// Reseed the dice per variant instead of rolling what the turn rolled.
    pub fresh_dice: bool,
}

impl Default for Spec {
    fn default() -> Self {
        Spec {
            turn: None,
            n: DEFAULT_N,
            director: None,
            effort: None,
            fresh_dice: false,
        }
    }
}

impl Spec {
    /// Parse `:reroll`'s argument: `[#seq] [n] [director=<model>]
    /// [effort=<level>] [dice=fresh|same]`, in any order.
    pub fn parse(arg: &str) -> Result<Spec, String> {
        let mut spec = Spec::default();
        for word in arg.split_whitespace() {
            if let Some(seq) = word.strip_prefix('#') {
                spec.turn = Some(seq.parse().map_err(|_| USAGE.to_string())?);
                continue;
            }
            if let Ok(n) = word.parse::<usize>() {
                if !(1..=MAX_N).contains(&n) {
                    return Err(format!("A reroll runs 1 to {MAX_N} variants."));
                }
                spec.n = n;
                continue;
            }
            match word.split_once('=') {
                Some(("director", m)) => {
                    spec.director = Some(Model::parse(m).ok_or_else(|| {
                        format!("Unknown model {m:?}: use haiku, sonnet or opus.")
                    })?);
                }
                Some(("effort", e)) => {
                    spec.effort = Some(Effort::parse(e).ok_or_else(|| {
                        format!("Unknown effort {e:?}: use low, medium, high, xhigh or max.")
                    })?);
                }
                Some(("dice", "fresh")) => spec.fresh_dice = true,
                Some(("dice", "same")) => spec.fresh_dice = false,
                _ => return Err(USAGE.into()),
            }
        }
        Ok(spec)
    }

    /// The knobs as stored in `rerolls.knobs`.
    pub fn knobs(&self) -> Value {
        let mut knobs = json!({});
        if let Some(m) = &self.director {
            knobs["director"] = json!(m.name());
        }
        if let Some(e) = self.effort {
            knobs["effort"] = json!(e.as_str());
        }
        knobs
    }

    /// `same` or `fresh`, as stored in `rerolls.dice`.
    pub fn dice(&self) -> &'static str {
        if self.fresh_dice { "fresh" } else { "same" }
    }

    /// The session's brains with this reroll's knobs applied.
    pub fn brains(&self, session: &Brains) -> Brains {
        let mut brains = session.clone();
        let mut change = String::from("director");
        if let Some(m) = &self.director {
            change = format!("director={}", m.name());
        }
        if let Some(e) = self.effort {
            change.push_str(&format!(" effort={}", e.as_str()));
        }
        if self.director.is_some() || self.effort.is_some() {
            brains
                .change(&change)
                .expect("knobs are parsed from known names");
        }
        brains
    }
}

/// `director haiku · effort high · same dice`: what a reroll ran with
/// (`✂ message surgery · …` for surgery).
pub fn describe_knobs(knobs: &Value, dice: &str) -> String {
    let mut parts = Vec::new();
    if let Some(target) = knobs["surgery"]["target"].as_str() {
        parts.push(format!("✂ {target} surgery"));
    }
    if let Some(m) = knobs["director"].as_str() {
        parts.push(format!("director {m}"));
    }
    if let Some(e) = knobs["effort"].as_str() {
        parts.push(format!("effort {e}"));
    }
    parts.push(format!("{dice} dice"));
    parts.join(" · ")
}

/// One thing a variant changed or did.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    /// What groups it with other variants' facts in the summary.
    pub key: String,
    /// How it reads in a variant's diff.
    pub text: String,
    /// Engine bookkeeping (`_` props and relations, time): shown in the
    /// full diff only.
    pub engine: bool,
}

impl Fact {
    fn new(text: impl Into<String>) -> Fact {
        let text = text.into();
        Fact {
            key: text.clone(),
            text,
            engine: false,
        }
    }

    fn keyed(key: impl Into<String>, text: impl Into<String>) -> Fact {
        Fact {
            key: key.into(),
            text: text.into(),
            engine: false,
        }
    }

    fn engine(text: impl Into<String>) -> Fact {
        Fact {
            engine: true,
            ..Fact::new(text)
        }
    }
}

/// A variant's turn that finished.
#[derive(Debug, Clone, PartialEq)]
pub struct Played {
    pub narration: String,
    /// `director` or `fast path (rule #4)`.
    pub route: String,
    pub facts: Vec<Fact>,
}

impl Played {
    /// The facts the compact diff shows (no engine bookkeeping).
    pub fn story_facts(&self) -> impl Iterator<Item = &Fact> {
        self.facts.iter().filter(|f| !f.engine)
    }
}

/// What sets a variant apart in a surgery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mark {
    /// A plain reroll variant.
    #[default]
    Plain,
    /// Played with the edited prompt: shown with ✂.
    Edited,
    /// Played with the prompt unedited, for comparison.
    Control,
}

impl Mark {
    /// ` ✂`, ` (control)` or nothing, after a variant's number.
    pub fn suffix(self) -> &'static str {
        match self {
            Mark::Plain => "",
            Mark::Edited => " ✂",
            Mark::Control => " (control)",
        }
    }
}

/// One finished variant, as the front ends show it.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantView {
    /// From 1.
    pub n: usize,
    /// Edited or control, in a surgery.
    pub mark: Mark,
    /// The turn, or why it failed.
    pub result: Result<Played, String>,
    /// Model calls it made.
    pub calls: usize,
    pub cost_usd: f64,
}

/// What a reroll is: shown as soon as it starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Head {
    /// The `rerolls` row.
    pub id: i64,
    /// The rerolled turn's input event.
    pub turn_seq: i64,
    pub input: String,
    pub n: usize,
    pub knobs: Value,
    pub dice: String,
}

impl Head {
    /// `Reroll r3 of turn #42 "kick the door" · 3 variants · same dice`
    /// (`Surgery r3 …` for a surgery).
    pub fn title(&self) -> String {
        format!(
            "{} r{} of turn #{} \"{}\" · {} variant{} · {}",
            if self.is_surgery() {
                "Surgery"
            } else {
                "Reroll"
            },
            self.id,
            self.turn_seq,
            xray::clip(&self.input, 60),
            self.n,
            if self.n == 1 { "" } else { "s" },
            describe_knobs(&self.knobs, &self.dice)
        )
    }

    /// Whether this is a `:surgery` (edited variants and a control).
    pub fn is_surgery(&self) -> bool {
        self.knobs["surgery"].is_object()
    }

    /// How variant `n` is marked: in a surgery the control is the variant
    /// its knobs name and the rest are edited.
    pub fn mark(&self, n: usize) -> Mark {
        match self.knobs["surgery"]["control"].as_u64() {
            None => Mark::Plain,
            Some(c) if c == n as u64 => Mark::Control,
            Some(_) => Mark::Edited,
        }
    }
}

/// Progress of a running reroll, in order: `Started`, then one `Variant`
/// per variant as each finishes (variant 1 first, the rest in any order).
#[derive(Debug)]
pub enum Progress<'a> {
    Started(&'a Head),
    Variant(&'a VariantView),
}

/// A finished reroll. Its variants' worlds are kept for [`Reroll::adopt`].
pub struct Reroll {
    pub head: Head,
    /// In variant order.
    pub variants: Vec<VariantView>,
    /// Each variant's world after its turn; `None` when the turn failed.
    worlds: Vec<Option<World>>,
}

impl std::fmt::Debug for Reroll {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reroll")
            .field("head", &self.head)
            .field("variants", &self.variants)
            .finish_non_exhaustive()
    }
}

impl Reroll {
    /// The summary across the variants.
    pub fn summary(&self) -> Summary {
        summarize(&self.variants)
    }

    /// Variant `n`'s world (from 1), if its turn finished.
    pub fn world(&self, n: usize) -> Option<&World> {
        self.worlds.get(n.checked_sub(1)?)?.as_ref()
    }

    /// Write variant `n`'s world into a new branch save at `path` beside
    /// the save that ran the reroll: same lineage, that save as parent,
    /// forked just before the rerolled turn, labelled `reroll r<id>.<n>`,
    /// with the variant's call records as the turn's own. Turns played
    /// after the rerolled one aren't in it. `Err` is what to tell the
    /// player.
    pub fn adopt(&self, n: usize, path: &std::path::Path) -> Result<(), String> {
        let Some(world) = self.world(n) else {
            return Err(match self.variants.get(n.wrapping_sub(1)) {
                None => format!("Reroll r{} has no variant {n}.", self.head.id),
                Some(_) => format!("Variant {n} failed; there is nothing to adopt."),
            });
        };
        if path.exists() {
            return Err(format!("{} already exists.", path.display()));
        }
        let branch = world
            .save_branch(path, self.head.turn_seq - 1)
            .map_err(|e| format!("[adopt failed: {e}]"))?;
        branch
            .set_label(&format!("reroll r{}.{n}", self.head.id))
            .map_err(|e| format!("[adopt failed: {e}]"))?;
        Ok(())
    }
}

/// Run `spec` against `world`, the save being played (left unchanged but
/// for call records and the `rerolls` row). `clients` gives each variant
/// (from 1) its model client; `progress` hears of the start and of each
/// variant as it finishes. `Err` is what to tell the player.
pub fn run<'c>(
    world: &World,
    spec: &Spec,
    session: &Brains,
    clients: &dyn Fn(usize) -> &'c (dyn LlmClient + Sync),
    progress: &mut dyn FnMut(Progress),
) -> Result<Reroll, String> {
    let plans = vec![Plan::Plain; spec.n];
    run_plans(
        world,
        spec,
        &spec.knobs(),
        &plans,
        session,
        clients,
        progress,
    )
}

/// How one variant plays its turn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Plan {
    /// The whole turn from the raw input, as it would play.
    Plain,
    /// Straight to the director with this prompt.
    Prompt(DirectorPrompt, Mark),
}

/// Run one variant per plan (`spec.n` is ignored: the plans say how many),
/// storing `knobs` in the `rerolls` row.
pub(crate) fn run_plans<'c>(
    world: &World,
    spec: &Spec,
    knobs: &Value,
    plans: &[Plan],
    session: &Brains,
    clients: &dyn Fn(usize) -> &'c (dyn LlmClient + Sync),
    progress: &mut dyn FnMut(Progress),
) -> Result<Reroll, String> {
    let n = plans.len();
    let failed = |e: StoreError| format!("[reroll failed: {e}]");
    let turn_seq = match spec.turn {
        Some(seq) => seq,
        None => xray::latest_turn(world)
            .map_err(failed)?
            .ok_or("No turn to reroll yet.")?,
    };
    let input = turn_input(world, turn_seq)
        .map_err(failed)?
        .ok_or_else(|| format!("Event #{turn_seq} isn't a turn's input; :xray shows turns."))?;
    if !(1..=MAX_N).contains(&n) {
        return Err(format!("A reroll runs 1 to {MAX_N} variants."));
    }
    let brains = spec.brains(session);
    let mut forks = Vec::with_capacity(n);
    for _ in 0..n {
        forks.push(
            world
                .fork_in_memory(turn_seq - 1)
                .map_err(|e| format!("[reroll failed: {e}]"))?,
        );
    }
    let id = world
        .insert_reroll(turn_seq, n, knobs, spec.dice())
        .map_err(failed)?;
    let head = Head {
        id,
        turn_seq,
        input: input.clone(),
        n,
        knobs: knobs.clone(),
        dice: spec.dice().into(),
    };
    progress(Progress::Started(&head));

    let mut variants: Vec<Option<VariantView>> = vec![None; n];
    let mut worlds: Vec<Option<World>> = (0..n).map(|_| None).collect();
    let mut take = |ran: Ran, progress: &mut dyn FnMut(Progress)| -> Result<(), String> {
        let k = ran.view.n;
        world
            .record_variant_calls(turn_seq, id, k as i64, &ran.calls)
            .map_err(failed)?;
        progress(Progress::Variant(&ran.view));
        if ran.view.result.is_ok() {
            worlds[k - 1] = Some(ran.world);
        }
        variants[k - 1] = Some(ran.view);
        Ok(())
    };

    let mut forks = forks.into_iter().enumerate().map(|(i, w)| (i + 1, w));
    // Variant 1 alone first: it writes the prompt cache the rest read.
    let (first_n, first) = forks.next().expect("at least one variant");
    take(
        play(
            first,
            first_n,
            &input,
            &plans[0],
            &brains,
            spec.fresh_dice,
            clients(first_n),
        ),
        progress,
    )?;
    let rest: Vec<_> = forks
        .map(|(k, w)| (k, w, &plans[k - 1], clients(k)))
        .collect();
    if !rest.is_empty() {
        let (tx, rx) = channel();
        std::thread::scope(|scope| -> Result<(), String> {
            for (k, fork, plan, client) in rest {
                let tx = tx.clone();
                let (input, brains) = (&input, &brains);
                let fresh = spec.fresh_dice;
                scope.spawn(move || {
                    let _ = tx.send(play(fork, k, input, plan, brains, fresh, client));
                });
            }
            drop(tx);
            for ran in rx {
                take(ran, progress)?;
            }
            Ok(())
        })?;
    }

    let variants: Vec<VariantView> = variants.into_iter().flatten().collect();
    let reroll = Reroll {
        head,
        variants,
        worlds,
    };
    let summary = summary_text(&reroll.summary(), n).join("\n");
    world.set_reroll_summary(id, &summary).map_err(failed)?;
    Ok(reroll)
}

/// The text the player typed for the turn whose input is event `seq`, or
/// `None` when that event isn't a turn's input.
pub(crate) fn turn_input(world: &World, seq: i64) -> Result<Option<String>, StoreError> {
    Ok(world
        .event(seq)?
        .filter(|e| e.kind == EventKind::Input && e.payload["seed"] != json!(true))
        .map(|e| e.payload["text"].as_str().unwrap_or_default().to_owned()))
}

/// A variant that ran: its world, what to show and its call records.
struct Ran {
    world: World,
    view: VariantView,
    calls: Vec<CallRecord>,
}

/// Play variant `n`: the turn from `input` on `fork`, with no tick.
fn play(
    mut fork: World,
    n: usize,
    input: &str,
    plan: &Plan,
    brains: &Brains,
    fresh_dice: bool,
    client: &dyn LlmClient,
) -> Ran {
    if fresh_dice {
        fork.set_dice_salt(n as u64);
    }
    let start = fork.last_seq();
    let before = fork.state();
    let mut game = Game::new(fork);
    game.set_brains(brains.clone());
    let (outcome, mark) = match plan {
        Plan::Plain => (game.turn(client, input, &mut |_| {}), Mark::Plain),
        Plan::Prompt(prompt, mark) => (
            game.turn_with_prompt(client, input, prompt, &mut |_| {}),
            *mark,
        ),
    };
    let world = game.into_world();
    let calls: Vec<CallRecord> = match world.turn_seqs().map(|s| s.last().copied()) {
        Ok(Some(seq)) if start.as_ref().is_ok_and(|&s| seq > s) => world
            .turn_calls(seq)
            .map(|c| c.into_iter().map(|c| c.call).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let result = match (outcome, start, before) {
        (Ok(outcome), Ok(start), Ok(before)) => {
            played(&world, start, &before, &outcome).map_err(|e| e.to_string())
        }
        (Err(e), ..) => Err(e.to_string()),
        (_, Err(e), _) | (.., Err(e)) => Err(e.to_string()),
    };
    Ran {
        view: VariantView {
            n,
            mark,
            result,
            calls: calls.len(),
            cost_usd: calls
                .iter()
                .filter_map(|c| c.cost_usd)
                .fold(0.0, |a, b| a + b),
        },
        world,
        calls,
    }
}

fn played(
    world: &World,
    start: i64,
    before: &WorldState,
    outcome: &TurnOutcome,
) -> Result<Played, StoreError> {
    Ok(Played {
        narration: outcome.narration.clone(),
        route: match outcome.fast_path {
            Some(rule) => format!("fast path (rule #{rule})"),
            None => "director".into(),
        },
        facts: facts(world, start, before)?,
    })
}

/// Kinds whose entities speak through their own events (rule changes) or
/// aren't story (the bible, the glossary).
const QUIET_KINDS: [&str; 3] = ["rule", "glossary", "bible"];

/// What changed in `world` since event `start`, when its state was
/// `before`: entities created, destroyed and moved, props and relations,
/// then the turn's rolls, refusals, rule changes, quest verdicts and time.
pub fn facts(world: &World, start: i64, before: &WorldState) -> Result<Vec<Fact>, StoreError> {
    let after = world.state()?;
    let old: BTreeMap<EntityId, &Entity> = before.entities.iter().map(|e| (e.id, e)).collect();
    let new: BTreeMap<EntityId, &Entity> = after.entities.iter().map(|e| (e.id, e)).collect();
    let name = |id: EntityId| -> String {
        new.get(&id)
            .or_else(|| old.get(&id))
            .map_or_else(|| format!("#{id}"), |e| e.name.clone())
    };
    let place = |at: Option<EntityId>| at.map_or_else(|| "nowhere".to_string(), name);
    let quiet = |e: &Entity| QUIET_KINDS.contains(&e.kind.as_str());
    let mut out = Vec::new();

    for e in after.entities.iter().filter(|e| !quiet(e)) {
        match old.get(&e.id) {
            None => out.push(Fact::new(format!(
                "+ {} ({}) in {}",
                e.name,
                e.kind,
                place(e.location)
            ))),
            Some(was) => {
                if was.location != e.location {
                    out.push(Fact::new(format!(
                        "{} moved: {} → {}",
                        e.name,
                        place(was.location),
                        place(e.location)
                    )));
                }
                let keys: BTreeSet<&String> = was.props.keys().chain(e.props.keys()).collect();
                for key in keys {
                    let (a, b) = (was.props.get(key), e.props.get(key));
                    if a == b {
                        continue;
                    }
                    let text = format!("{}.{key}: {} → {}", e.name, show(a), show(b));
                    out.push(if key.starts_with(ENGINE_PROP_PREFIX) {
                        Fact::engine(text)
                    } else {
                        Fact::new(text)
                    });
                }
            }
        }
        if let Some(was) = old.get(&e.id)
            && was.name != e.name
        {
            out.push(Fact::new(format!("{} renamed {}", was.name, e.name)));
        }
    }
    for e in before.entities.iter().filter(|e| !quiet(e)) {
        if !new.contains_key(&e.id) {
            out.push(Fact::new(format!("− {} ({})", e.name, e.kind)));
        }
    }

    let key = |r: &Relation| (r.subject, r.rel.clone(), r.object);
    let old_rels: BTreeMap<_, &Relation> = before.relations.iter().map(|r| (key(r), r)).collect();
    let new_rels: BTreeMap<_, &Relation> = after.relations.iter().map(|r| (key(r), r)).collect();
    let relation = |sign: char, r: &Relation| {
        let text = format!("{sign} {} {} {}", name(r.subject), r.rel, name(r.object));
        if r.rel.starts_with(ENGINE_PROP_PREFIX) {
            Fact::engine(text)
        } else {
            Fact::new(text)
        }
    };
    for (k, r) in &new_rels {
        if !old_rels.contains_key(k) {
            out.push(relation('+', r));
        }
    }
    for (k, r) in &old_rels {
        if !new_rels.contains_key(k) {
            out.push(relation('−', r));
        }
    }

    // A rejected patch is logged inside the tool call that tried it, whose
    // error says the same again: keep one line, as the X-ray does.
    let mut explained = false;
    for e in world.events_since(start)? {
        let p = &e.payload;
        match e.kind {
            EventKind::Roll => out.push(Fact::new(format!(
                "roll {}: d20 {} + {} = {} vs {}: {}",
                p["stat"].as_str().unwrap_or("?"),
                p["die"],
                p["value"],
                p["total"],
                p["difficulty"],
                p["result"].as_str().unwrap_or("?")
            ))),
            EventKind::PatchRejected => {
                out.push(Fact::keyed("refused", xray::refusal(&e)?));
                explained = true;
            }
            EventKind::ToolCall => {
                if p["is_error"] == json!(true) && !explained {
                    let tool = p["name"].as_str().unwrap_or("?");
                    out.push(Fact::keyed(
                        "refused",
                        format!(
                            "refused: {tool}: {}",
                            xray::clip(p["result"].as_str().unwrap_or_default(), 100)
                        ),
                    ));
                }
                explained = false;
            }
            EventKind::RuleChange => {
                let action = p["action"].as_str().unwrap_or("changed");
                out.push(Fact::keyed(
                    format!("rule {action}"),
                    format!(
                        "rule {action}: {}",
                        p["rule"]["name"].as_str().unwrap_or("?")
                    ),
                ));
            }
            EventKind::Judgment if p["quest"].is_i64() => out.push(Fact::new(format!(
                "quest \"{}\": {}",
                p["title"].as_str().unwrap_or("?"),
                p["verdict"].as_str().unwrap_or("?")
            ))),
            EventKind::Time => out.push(Fact::engine(format!(
                "time +{} min ({})",
                p["minutes"],
                p["source"].as_str().unwrap_or("?")
            ))),
            _ => {}
        }
    }
    Ok(out)
}

/// A prop value, short: strings bare, anything else as compact JSON, `—`
/// when absent.
fn show(v: Option<&Value>) -> String {
    match v {
        None => "—".into(),
        Some(Value::String(s)) => xray::clip(s, 40),
        Some(v) => xray::clip(&v.to_string(), 40),
    }
}

/// Key of the fact a variant that changed nothing gets in the summary.
pub const NO_CHANGE: &str = "nothing changed";

/// What the variants have in common and where they split.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Summary {
    /// Each route and the variants that took it, most common first.
    pub routes: Vec<(String, Vec<usize>)>,
    /// Each fact key and the variants it happened in, most common first.
    pub facts: Vec<(String, Vec<usize>)>,
    /// Variants whose turn failed.
    pub failed: Vec<usize>,
    /// Variants finished so far.
    pub done: usize,
    pub cost_usd: f64,
}

/// Group the finished variants' routes and facts. Engine bookkeeping is
/// left out; a variant with no story facts counts as [`NO_CHANGE`].
pub fn summarize(variants: &[VariantView]) -> Summary {
    let mut s = Summary {
        done: variants.len(),
        cost_usd: variants.iter().map(|v| v.cost_usd).sum(),
        ..Summary::default()
    };
    let add = |groups: &mut Vec<(String, Vec<usize>)>, key: &str, n: usize| match groups
        .iter_mut()
        .find(|(k, _)| k == key)
    {
        Some((_, ns)) if ns.contains(&n) => {}
        Some((_, ns)) => ns.push(n),
        None => groups.push((key.to_owned(), vec![n])),
    };
    let mut ordered: Vec<&VariantView> = variants.iter().collect();
    ordered.sort_by_key(|v| v.n);
    for v in ordered {
        match &v.result {
            Err(_) => s.failed.push(v.n),
            Ok(p) => {
                add(&mut s.routes, &p.route, v.n);
                let mut any = false;
                for f in p.story_facts() {
                    add(&mut s.facts, &f.key, v.n);
                    any = true;
                }
                if !any {
                    add(&mut s.facts, NO_CHANGE, v.n);
                }
            }
        }
    }
    // Stable: ties keep the order first seen.
    s.routes.sort_by_key(|(_, ns)| std::cmp::Reverse(ns.len()));
    s.facts.sort_by_key(|(_, ns)| std::cmp::Reverse(ns.len()));
    s
}

/// The summary as lines, counts out of `of` variants:
///
/// ```text
/// route: director 2/3 · fast path (rule #4) 1/3
/// 3/3  door.broken: false → true
/// 1/3  rule minted   (variant 2)
/// ```
pub fn summary_text(s: &Summary, of: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if !s.routes.is_empty() {
        lines.push(format!(
            "route: {}",
            s.routes
                .iter()
                .map(|(r, ns)| format!("{r} {}/{of}", ns.len()))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }
    for (key, ns) in &s.facts {
        let mut line = format!("{}/{of}  {key}", ns.len());
        if ns.len() < s.done - s.failed.len() {
            line.push_str(&format!("  ({})", which(ns)));
        }
        lines.push(line);
    }
    if !s.failed.is_empty() {
        lines.push(format!("failed: {}", which(&s.failed)));
    }
    lines
}

/// `variant 2` or `variants 1, 3`.
fn which(ns: &[usize]) -> String {
    let list = ns
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if ns.len() == 1 {
        format!("variant {list}")
    } else {
        format!("variants {list}")
    }
}

/// `Variant 2 · director · 3 calls · $0.004`
pub fn variant_heading(v: &VariantView) -> String {
    let route = match &v.result {
        Ok(p) => p.route.as_str(),
        Err(_) => "failed",
    };
    format!(
        "Variant {}{} · {route} · {} call{} · ${:.3}",
        v.n,
        v.mark.suffix(),
        v.calls,
        if v.calls == 1 { "" } else { "s" },
        v.cost_usd
    )
}

/// A variant's diff as lines: the story facts, or every fact with
/// `engine`.
pub fn diff_lines(v: &VariantView, engine: bool) -> Vec<String> {
    match &v.result {
        Err(e) => vec![format!("failed: {e}")],
        Ok(p) => {
            let lines: Vec<String> = p
                .facts
                .iter()
                .filter(|f| engine || !f.engine)
                .map(|f| f.text.clone())
                .collect();
            if lines.is_empty() {
                vec![format!("({NO_CHANGE})")]
            } else {
                lines
            }
        }
    }
}

/// The reroll's head, total cost and summary lines: the top of the TUI
/// view and of the plain report.
pub fn overview(head: &Head, variants: &[VariantView]) -> Vec<String> {
    let s = summarize(variants);
    let mut lines = vec![head.title()];
    let progress = if s.done < head.n {
        format!(" · {}/{} done", s.done, head.n)
    } else {
        String::new()
    };
    lines.push(format!("total ${:.3}{progress}", s.cost_usd));
    if let Some(diff) = head.knobs["surgery"]["diff"].as_str() {
        lines.extend(surgery::diff_preview(diff));
    }
    lines.extend(summary_text(&s, head.n));
    lines
}

/// Plain mode's report: the overview, then every variant stacked with its
/// narration and diff.
pub fn report_text(head: &Head, variants: &[VariantView]) -> String {
    let mut out = overview(head, variants).join("\n");
    let mut ordered: Vec<&VariantView> = variants.iter().collect();
    ordered.sort_by_key(|v| v.n);
    for v in ordered {
        write!(out, "\n\n── {}\n", variant_heading(v)).unwrap();
        if let Ok(p) = &v.result {
            out.push_str(&p.narration);
            out.push('\n');
        }
        for line in diff_lines(v, false) {
            write!(out, "\n  {line}").unwrap();
        }
    }
    write!(
        out,
        "\n\n:reroll show <n> reads a variant; :adopt <n> plays it as a new branch; \
         :context r{}.<n> lists its calls.",
        head.id
    )
    .unwrap();
    out
}

/// `:reroll show <n>`: a variant's whole narration and full diff.
pub fn variant_text(v: &VariantView) -> String {
    let mut out = variant_heading(v);
    out.push_str("\n\n");
    if let Ok(p) = &v.result {
        out.push_str(&p.narration);
        out.push_str("\n\n");
    }
    out.push_str(&diff_lines(v, true).join("\n"));
    out
}

/// `:rerolls`: every reroll run on this save, latest last.
pub fn list(world: &World) -> Result<String, StoreError> {
    let rows = world.rerolls()?;
    if rows.is_empty() {
        return Ok("No rerolls yet. :reroll (Ctrl-R) reruns the last turn.".into());
    }
    let mut out = String::new();
    for r in &rows {
        let input = turn_input(world, r.turn_seq)?.unwrap_or_default();
        writeln!(
            out,
            "r{} · turn #{} \"{}\" · {} variant{} · {} · ${:.3}",
            r.id,
            r.turn_seq,
            xray::clip(&input, 40),
            r.n,
            if r.n == 1 { "" } else { "s" },
            describe_knobs(&r.knobs, &r.dice),
            r.cost_usd
        )
        .unwrap();
        match &r.summary {
            Some(s) => {
                for line in s.lines() {
                    writeln!(out, "    {line}").unwrap();
                }
            }
            None => out.push_str("    (didn't finish)\n"),
        }
        if let Some(diff) = r.knobs["surgery"]["diff"].as_str() {
            writeln!(out, "    ✂ edit:").unwrap();
            for line in diff.lines() {
                writeln!(out, "      {line}").unwrap();
            }
        }
    }
    out.push_str(
        "\n:context r<id>.<n> lists a variant's calls. Only the latest reroll this session \
         can be adopted.",
    );
    Ok(out)
}
