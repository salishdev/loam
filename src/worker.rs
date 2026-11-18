//! The game on its own thread.
//!
//! A [`Worker`] owns the [`Game`] (and so the writing connection to the save)
//! and the model client. A front end sends it [`Command`]s and receives
//! [`Event`]s: narration as it streams, then what the command produced, then
//! [`Event::Ready`] once the worker is idle again. Views read the save
//! through a second, read-only connection ([`World::open_read_only`]), so
//! they keep working while a turn runs.
//!
//! [`World::open_read_only`]: crate::world::World::open_read_only

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use crate::game::{Game, GameError, TickOutcome, TurnOutcome};
use crate::llm::LlmClient;

/// What a front end asks the worker to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Build the world bible and stat sheet from a seed prompt.
    CreateBible(String),
    /// Generate the opening room and narrate the opening scene.
    Begin,
    /// Play one turn; a world tick follows when one is due.
    Turn(String),
    /// Run a world tick now (`:tick`).
    Tick,
    /// Stop the worker; the game and its save are dropped.
    Quit,
}

/// What the worker reports back, in order.
#[derive(Debug)]
pub enum Event {
    /// A piece of streamed narration.
    Text(String),
    /// [`Command::CreateBible`] finished.
    BibleDone(Result<(), GameError>),
    /// A turn (or, with `opening`, the opening scene) finished.
    TurnDone {
        opening: bool,
        outcome: Result<TurnOutcome, GameError>,
    },
    /// A world tick ran: `forced` by [`Command::Tick`], or due after a turn.
    TickDone {
        forced: bool,
        outcome: Result<TickOutcome, GameError>,
    },
    /// Progress of a reroll's variants (filled in by the reroll work).
    RerollProgress { done: usize, of: usize },
    /// The worker finished a command and waits for the next.
    Ready,
}

/// A running worker thread.
pub struct Worker {
    commands: Sender<Command>,
    events: Receiver<Event>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Start a worker that plays `game` against `client`.
    pub fn spawn(game: Game, client: Box<dyn LlmClient + Send>) -> Worker {
        let (commands, inbox) = channel();
        let (outbox, events) = channel();
        let thread = std::thread::Builder::new()
            .name("loam-game".into())
            .spawn(move || serve(game, client.as_ref(), &inbox, &outbox))
            .expect("spawn the game thread");
        Worker {
            commands,
            events,
            thread: Some(thread),
        }
    }

    /// Queue a command. Commands run one at a time, in order.
    pub fn send(&self, command: Command) {
        // A worker that has stopped has dropped its inbox; nothing to do.
        let _ = self.commands.send(command);
    }

    /// The next event, if one is waiting.
    pub fn try_recv(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Wait for the next event; `None` once the worker has stopped.
    pub fn recv(&self) -> Option<Event> {
        self.events.recv().ok()
    }

    /// Ask the worker to stop and wait for it, so the save closes cleanly.
    pub fn shut_down(mut self) {
        self.send(Command::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(game: Game, client: &dyn LlmClient, inbox: &Receiver<Command>, out: &Sender<Event>) {
    let mut game = game;
    while let Ok(command) = inbox.recv() {
        let mut stream = |t: &str| {
            let _ = out.send(Event::Text(t.to_string()));
        };
        match command {
            Command::Quit => break,
            Command::CreateBible(seed) => {
                let _ = out.send(Event::BibleDone(game.create_bible(client, &seed)));
            }
            Command::Begin => {
                let outcome = game.begin(client, &mut stream);
                let _ = out.send(Event::TurnDone {
                    opening: true,
                    outcome,
                });
            }
            Command::Turn(input) => {
                let outcome = game.turn(client, &input, &mut stream);
                let played = outcome.is_ok();
                let _ = out.send(Event::TurnDone {
                    opening: false,
                    outcome,
                });
                // The world moves on off-screen; a failed tick is retried
                // after the next turn.
                if played && let Some(outcome) = game.maybe_tick(client).transpose() {
                    let _ = out.send(Event::TickDone {
                        forced: false,
                        outcome,
                    });
                }
            }
            Command::Tick => {
                let _ = out.send(Event::TickDone {
                    forced: true,
                    outcome: game.tick(client),
                });
            }
        }
        if out.send(Event::Ready).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::tests::begun;
    use crate::llm::{Response, ScriptedClient};
    use crate::world::World;

    /// Collects events up to and including the next `Ready`.
    fn until_ready(worker: &Worker) -> Vec<Event> {
        let mut seen = Vec::new();
        while let Some(e) = worker.recv() {
            let ready = matches!(e, Event::Ready);
            seen.push(e);
            if ready {
                return seen;
            }
        }
        panic!("worker stopped before Ready: {seen:?}");
    }

    fn text(events: &[Event]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_turn_streams_text_then_reports_and_goes_idle() {
        let (game, _) = begun();
        let client = ScriptedClient::new([Response::canned_text("The candle gutters.")]);
        let worker = Worker::spawn(game, Box::new(client));
        worker.send(Command::Turn("look".into()));
        let events = until_ready(&worker);
        assert_eq!(text(&events), "The candle gutters.");
        let done = events
            .iter()
            .position(|e| matches!(e, Event::TurnDone { opening: false, outcome: Ok(o) } if o.narration == "The candle gutters."))
            .expect("turn done");
        let last_text = events
            .iter()
            .rposition(|e| matches!(e, Event::Text(_)))
            .unwrap();
        assert!(last_text < done, "{events:?}");
        assert!(matches!(events.last(), Some(Event::Ready)));
        worker.shut_down();
    }

    #[test]
    fn a_failed_turn_is_reported_and_the_worker_carries_on() {
        let (game, _) = begun();
        let client = ScriptedClient::new([]);
        let worker = Worker::spawn(game, Box::new(client));
        worker.send(Command::Turn("look".into()));
        let events = until_ready(&worker);
        assert!(
            events.iter().any(|e| matches!(
                e,
                Event::TurnDone {
                    outcome: Err(_),
                    ..
                }
            )),
            "{events:?}"
        );
        // No tick after a failed turn.
        assert!(!events.iter().any(|e| matches!(e, Event::TickDone { .. })));
        worker.send(Command::Tick);
        let events = until_ready(&worker);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TickDone { forced: true, .. })),
            "{events:?}"
        );
        worker.shut_down();
    }

    #[test]
    fn a_new_world_is_built_and_opened_through_the_worker() {
        use crate::game::tests::{bible_call, opening_room};
        let client = ScriptedClient::new([
            bible_call(),
            Response::canned_text(""),
            opening_room(),
            Response::canned_text("You wake in a flooded chapel."),
        ]);
        let mut game = Game::new(World::open_in_memory().unwrap());
        game.set_dice_seed(7);
        let worker = Worker::spawn(game, Box::new(client));
        worker.send(Command::CreateBible("A drowned abbey.".into()));
        let events = until_ready(&worker);
        assert!(matches!(events[0], Event::BibleDone(Ok(()))), "{events:?}");
        worker.send(Command::Begin);
        let events = until_ready(&worker);
        assert_eq!(text(&events), "You wake in a flooded chapel.");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::TurnDone {
                opening: true,
                outcome: Ok(_)
            }
        )));
        worker.shut_down();
    }

    #[test]
    fn a_read_only_view_sees_what_the_worker_wrote() {
        let dir = std::env::temp_dir().join(format!("loam-worker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("view.db");
        let _ = std::fs::remove_file(&path);
        // Build a begun game in memory, then fork it into a file save.
        let (game, _) = begun();
        let last = game.world().last_seq().unwrap();
        game.world().fork(last, &path).unwrap();
        let game = Game::new(World::open(&path).unwrap());
        let view = World::open_read_only(&path).unwrap();
        let before = view.last_seq().unwrap();

        let client = ScriptedClient::new([Response::canned_text("Reeds whisper.")]);
        let worker = Worker::spawn(game, Box::new(client));
        worker.send(Command::Turn("listen".into()));
        until_ready(&worker);
        assert!(view.last_seq().unwrap() > before);
        let log = crate::inspect::log(&view, "narration").unwrap();
        assert!(log.contains("Reeds whisper"), "{log}");
        worker.shut_down();
        drop(view);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
