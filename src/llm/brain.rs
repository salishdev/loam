//! Brains: which model, at which effort, each role's calls go to.
//!
//! [`DEFAULTS`] is the one place a role's model is chosen; call sites build
//! their requests with [`Request::for_role`]. `:brain` changes a role for
//! the session through [`Brains`], which [`BrainClient`] applies to every
//! request on its way to the model (and so to the call records). Nothing
//! here is saved.

use std::fmt::Write as _;

use super::{Effort, LlmClient, LlmError, Message, Model, Request, Response};

/// A role's model and effort. No effort leaves the model's own default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brain {
    pub model: Model,
    pub effort: Option<Effort>,
}

impl Brain {
    const fn new(model: Model, effort: Option<Effort>) -> Brain {
        Brain { model, effort }
    }

    fn effort_name(&self) -> &'static str {
        self.effort.map_or("default", Effort::as_str)
    }
}

/// Every role, in the order `:brain` lists them, with its default brain.
pub static DEFAULTS: [(&str, Brain); 6] = [
    ("director", Brain::new(Model::Sonnet, Some(Effort::Medium))),
    ("narrator", Brain::new(Model::Sonnet, None)),
    ("interpret", Brain::new(Model::Haiku, None)),
    ("judge", Brain::new(Model::Haiku, None)),
    ("tick", Brain::new(Model::Sonnet, Some(Effort::Medium))),
    ("bible", Brain::new(Model::Sonnet, Some(Effort::Medium))),
];

/// The roles, in [`DEFAULTS`] order.
pub fn roles() -> impl Iterator<Item = &'static str> {
    DEFAULTS.iter().map(|(r, _)| *r)
}

fn default_for(role: &str) -> Option<&'static Brain> {
    DEFAULTS.iter().find(|(r, _)| *r == role).map(|(_, b)| b)
}

impl Request {
    /// A request for `role` with that role's default model and effort.
    ///
    /// # Panics
    /// When `role` isn't in [`DEFAULTS`].
    pub fn for_role(role: &'static str, messages: Vec<Message>) -> Request {
        let brain = default_for(role).unwrap_or_else(|| panic!("no default brain for {role}"));
        let mut request = Request::new(brain.model.clone(), messages).role(role);
        request.effort = brain.effort;
        request
    }
}

/// The session's brain for every role: the defaults plus `:brain` changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brains {
    table: Vec<(&'static str, Brain)>,
}

impl Default for Brains {
    fn default() -> Self {
        Brains {
            table: DEFAULTS.to_vec(),
        }
    }
}

/// What `:brain` accepts, for its error messages.
pub const USAGE: &str = "Usage: :brain [<role>=<haiku|sonnet|opus>]… [effort=<low|medium|high|xhigh|max>] \
| :brain <role> effort=<level> | :brain reset";

impl Brains {
    /// The roles and their brains, in [`DEFAULTS`] order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &Brain)> {
        self.table.iter().map(|(r, b)| (*r, b))
    }

    /// The brain for `role`, if it is one.
    pub fn get(&self, role: &str) -> Option<&Brain> {
        self.table.iter().find(|(r, _)| *r == role).map(|(_, b)| b)
    }

    fn get_mut(&mut self, role: &str) -> Option<&mut Brain> {
        self.table
            .iter_mut()
            .find(|(r, _)| *r == role)
            .map(|(_, b)| b)
    }

    /// The director's model, for the status bar.
    pub fn director_model(&self) -> &str {
        self.get("director").map_or("?", |b| b.model.name())
    }

    /// Whether `role` differs from its default.
    pub fn changed(&self, role: &str) -> bool {
        self.get(role) != default_for(role)
    }

    /// Give `request` its role's brain. Requests without a known role are
    /// left alone.
    pub fn apply(&self, request: &mut Request) {
        if let Some(brain) = request.role.and_then(|r| self.get(r)) {
            request.model = brain.model.clone();
            request.effort = brain.effort;
        }
    }

    /// The role at `index` in [`DEFAULTS`] order.
    pub fn role_at(&self, index: usize) -> Option<&'static str> {
        self.table.get(index).map(|(r, _)| *r)
    }

    /// Move `role` to the next model (haiku → sonnet → opus → haiku).
    pub fn cycle_model(&mut self, role: &str) {
        if let Some(b) = self.get_mut(role) {
            let at = Model::CHOICES.iter().position(|m| *m == b.model);
            let next = at.map_or(0, |i| (i + 1) % Model::CHOICES.len());
            b.model = Model::CHOICES[next].clone();
        }
    }

    /// Move `role` to the next effort (low → … → max → low; the model
    /// default goes to low).
    pub fn cycle_effort(&mut self, role: &str) {
        if let Some(b) = self.get_mut(role) {
            let at = b
                .effort
                .and_then(|e| Effort::ALL.iter().position(|x| *x == e));
            let next = at.map_or(0, |i| (i + 1) % Effort::ALL.len());
            b.effort = Some(Effort::ALL[next]);
        }
    }

    /// Apply `:brain`'s argument: `director=opus effort=high`,
    /// `interpret=sonnet`, `director effort=max`, `reset`. `effort=` applies
    /// to every role the same command names. Nothing changes on an error.
    pub fn change(&mut self, arg: &str) -> Result<(), String> {
        if arg.trim() == "reset" {
            *self = Brains::default();
            return Ok(());
        }
        let mut next = self.clone();
        let mut named: Vec<&str> = Vec::new();
        let mut effort = None;
        for word in arg.split_whitespace() {
            let (key, value) = match word.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (word, None),
            };
            if key == "effort" {
                let value = value.unwrap_or_default();
                effort = Some(Effort::parse(value).ok_or_else(|| {
                    format!("Unknown effort {value:?}: use low, medium, high, xhigh or max.")
                })?);
                continue;
            }
            let Some(role) = roles().find(|r| *r == key) else {
                return Err(format!(
                    "Unknown role {key:?}: the roles are {}.",
                    roles().collect::<Vec<_>>().join(", ")
                ));
            };
            if let Some(value) = value {
                let model = Model::parse(value).ok_or_else(|| {
                    format!("Unknown model {value:?}: use haiku, sonnet or opus.")
                })?;
                next.get_mut(role).expect("a known role").model = model;
            }
            named.push(role);
        }
        if named.is_empty() {
            return Err(USAGE.into());
        }
        if let Some(effort) = effort {
            for role in named {
                next.get_mut(role).expect("a known role").effort = Some(effort);
            }
        }
        *self = next;
        Ok(())
    }

    /// The table `:brain` shows: one line per role after a header line,
    /// changed roles marked `*`.
    pub fn table(&self) -> String {
        let mut out = format!("  {:<10} {:<7} effort\n", "role", "model");
        for (role, brain) in self.iter() {
            let mark = if self.changed(role) { '*' } else { ' ' };
            writeln!(
                out,
                "{mark} {role:<10} {:<7} {}",
                brain.model.name(),
                brain.effort_name()
            )
            .unwrap();
        }
        out.push_str("\n* changed this session (not saved). :brain reset restores the defaults.");
        out
    }
}

/// Lines in [`Brains::table`] before the first role.
pub const TABLE_HEADER_LINES: usize = 1;

/// Wraps a client and gives each request its role's brain.
pub struct BrainClient<'a> {
    brains: &'a Brains,
    inner: &'a dyn LlmClient,
}

impl<'a> BrainClient<'a> {
    pub fn new(brains: &'a Brains, inner: &'a dyn LlmClient) -> Self {
        BrainClient { brains, inner }
    }
}

impl LlmClient for BrainClient<'_> {
    fn send(&self, request: &Request, on_text: &mut dyn FnMut(&str)) -> Result<Response, LlmError> {
        if *self.brains == Brains::default() {
            return self.inner.send(request, on_text);
        }
        let mut request = request.clone();
        self.brains.apply(&mut request);
        self.inner.send(&request, on_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ScriptedClient;

    #[test]
    fn defaults_are_todays_models() {
        let b = Brains::default();
        assert_eq!(b.director_model(), "sonnet");
        assert_eq!(b.get("interpret"), Some(&Brain::new(Model::Haiku, None)));
        let r = Request::for_role("director", vec![]);
        assert_eq!(
            (r.model, r.effort, r.role),
            (Model::Sonnet, Some(Effort::Medium), Some("director"))
        );
        assert!(roles().all(|r| !b.changed(r)));
    }

    #[test]
    fn changes_parse_and_apply_effort_to_the_named_roles() {
        let mut b = Brains::default();
        b.change("director=opus effort=high").unwrap();
        assert_eq!(
            b.get("director"),
            Some(&Brain::new(Model::Opus, Some(Effort::High)))
        );
        b.change("interpret=sonnet").unwrap();
        assert_eq!(b.get("interpret"), Some(&Brain::new(Model::Sonnet, None)));
        b.change("judge effort=max").unwrap();
        assert_eq!(
            b.get("judge"),
            Some(&Brain::new(Model::Haiku, Some(Effort::Max)))
        );
        assert!(b.changed("director") && !b.changed("tick"));
        assert_eq!(b.director_model(), "opus");
        b.change("reset").unwrap();
        assert_eq!(b, Brains::default());
    }

    #[test]
    fn parse_errors_change_nothing() {
        let mut b = Brains::default();
        for (arg, wanted) in [
            ("director=gpt", "Unknown model \"gpt\""),
            ("wizard=opus", "Unknown role \"wizard\""),
            ("director=opus effort=huge", "Unknown effort \"huge\""),
            ("effort=high", "Usage: :brain"),
            ("", "Usage: :brain"),
        ] {
            let err = b.change(arg).unwrap_err();
            assert!(err.starts_with(wanted), "{arg}: {err}");
        }
        assert_eq!(b, Brains::default());
    }

    #[test]
    fn cycling_wraps_around() {
        let mut b = Brains::default();
        b.cycle_model("director");
        assert_eq!(b.get("director").unwrap().model, Model::Opus);
        b.cycle_model("director");
        assert_eq!(b.get("director").unwrap().model, Model::Haiku);
        b.cycle_effort("narrator");
        assert_eq!(b.get("narrator").unwrap().effort, Some(Effort::Low));
        b.cycle_effort("director");
        assert_eq!(b.get("director").unwrap().effort, Some(Effort::High));
        for _ in 0..3 {
            b.cycle_effort("director");
        }
        assert_eq!(b.get("director").unwrap().effort, Some(Effort::Low));
    }

    #[test]
    fn the_table_marks_changed_roles() {
        let mut b = Brains::default();
        b.change("tick=haiku effort=low").unwrap();
        let table = b.table();
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["role", "model", "effort"]
        );
        assert_eq!(
            lines[TABLE_HEADER_LINES]
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["director", "sonnet", "medium"]
        );
        assert_eq!(
            lines[TABLE_HEADER_LINES + 4]
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["*", "tick", "haiku", "low"]
        );
        assert!(lines[TABLE_HEADER_LINES + 1].contains("default"));
    }

    #[test]
    fn the_client_rewrites_requests_by_role() {
        let mut b = Brains::default();
        b.change("narrator=opus effort=xhigh").unwrap();
        let inner = ScriptedClient::new([
            Response::canned_text("a"),
            Response::canned_text("b"),
            Response::canned_text("c"),
        ]);
        let client = BrainClient::new(&b, &inner);
        client
            .complete(&Request::for_role("narrator", vec![Message::user("x")]))
            .unwrap();
        client
            .complete(&Request::for_role("judge", vec![Message::user("x")]))
            .unwrap();
        client
            .complete(&Request::new(Model::Haiku, vec![Message::user("x")]))
            .unwrap();
        let seen = inner.requests();
        assert_eq!(
            (&seen[0].model, seen[0].effort),
            (&Model::Opus, Some(Effort::Xhigh))
        );
        assert_eq!((&seen[1].model, seen[1].effort), (&Model::Haiku, None));
        assert_eq!((&seen[2].model, seen[2].role), (&Model::Haiku, None));
    }
}
