use serde_json::{Map, json};

use super::*;
use crate::world::{Op, Patch};

/// A world with just a bible, its clock started at `start` when given.
fn with_bible(start: Option<u64>) -> World {
    let mut world = World::open_in_memory().unwrap();
    let props = match start {
        Some(s) => Map::from_iter([start_prop(s)]),
        None => Map::new(),
    };
    world
        .apply_engine_patch(&Patch::new(vec![Op::Create {
            handle: None,
            name: "World bible".into(),
            kind: crate::game::BIBLE_KIND.into(),
            location: None,
            props,
        }]))
        .unwrap();
    world
}

#[test]
fn moments_show_day_time_and_part_of_day() {
    assert_eq!(Moment(0).to_string(), "day 1, 00:00 (night)");
    assert_eq!(Moment(8 * 60).to_string(), "day 1, 08:00 (morning)");
    assert_eq!(
        Moment(DAY + 18 * 60 + 40).to_string(),
        "day 2, 18:40 (dusk)"
    );
    let parts: Vec<_> = (0..24).map(|h| Moment(h * 60).part()).collect();
    assert_eq!(
        parts,
        [
            "night",
            "night",
            "night",
            "night",
            "night",
            "dawn",
            "dawn",
            "morning",
            "morning",
            "morning",
            "morning",
            "morning",
            "afternoon",
            "afternoon",
            "afternoon",
            "afternoon",
            "afternoon",
            "dusk",
            "dusk",
            "evening",
            "evening",
            "evening",
            "night",
            "night"
        ]
    );
}

#[test]
fn spans_read_in_days_hours_and_minutes() {
    assert_eq!(span(0), "0 min");
    assert_eq!(span(10), "10 min");
    assert_eq!(span(60), "1 h");
    assert_eq!(span(125), "2 h 5 min");
    assert_eq!(span(DAY + 180), "1 day 3 h");
    assert_eq!(span(2 * DAY + 1), "2 days 1 min");
}

#[test]
fn start_times_parse_as_24_hour_hh_mm() {
    assert_eq!(parse_hhmm("08:00"), Some(480));
    assert_eq!(parse_hhmm("7:05"), Some(425));
    assert_eq!(parse_hhmm(" 23:59 "), Some(23 * 60 + 59));
    for bad in [
        "24:00", "12:60", "12:5", "noon", "12", ":30", "-1:00", "123:00",
    ] {
        assert_eq!(parse_hhmm(bad), None, "{bad}");
    }
}

#[test]
fn a_world_without_a_clock_reads_as_day_one_morning() {
    let world = World::open_in_memory().unwrap();
    assert_eq!(now(&world).unwrap(), Moment(DEFAULT_START));
    // An old save: a bible with no clock.
    let world = with_bible(None);
    assert_eq!(now(&world).unwrap(), Moment(DEFAULT_START));
    assert_eq!(slice_line(&world).unwrap(), "Time: day 1, 08:00 (morning)");
}

#[test]
fn advancing_patches_the_clock_and_logs_who_decided() {
    let mut world = with_bible(Some(23 * 60 + 50));
    let to = advance(&mut world, 20, Source::Director).unwrap();
    assert_eq!(to.to_string(), "day 2, 00:10 (night)");
    assert_eq!(now(&world).unwrap(), to);

    let events = world.events().unwrap();
    let n = events.len();
    assert_eq!(events[n - 2].kind, EventKind::Patch);
    assert_eq!(events[n - 1].kind, EventKind::Time);
    assert_eq!(
        events[n - 1].payload,
        json!({"minutes": 20, "source": "director", "minute": DAY + 10})
    );
    advance(&mut world, 5, Source::Default).unwrap();
    assert_eq!(
        recent(&world, 5).unwrap(),
        vec![
            Advance {
                seq: n as i64 + 2,
                minutes: 5,
                source: "default".into(),
                to: Moment(DAY + 15),
            },
            Advance {
                seq: n as i64,
                minutes: 20,
                source: "director".into(),
                to: Moment(DAY + 10),
            },
        ]
    );
    assert_eq!(
        slice_line(&world).unwrap(),
        "Time: day 2, 00:15 (night); your last action took 5 min"
    );
    // The clock replays from the log.
    let replayed = world.replay_in_memory().unwrap();
    assert_eq!(now(&replayed).unwrap(), Moment(DAY + 15));
}

#[test]
fn advances_are_capped_and_need_a_bible() {
    let mut world = with_bible(None);
    advance(&mut world, 30 * DAY, Source::Director).unwrap();
    assert_eq!(now(&world).unwrap(), Moment(DEFAULT_START + MAX_ADVANCE));

    let mut empty = World::open_in_memory().unwrap();
    assert_eq!(
        advance(&mut empty, 10, Source::Default).unwrap(),
        Moment(DEFAULT_START)
    );
    assert!(empty.events().unwrap().is_empty());
}

#[test]
fn buckets_and_pass_time() {
    assert_eq!(
        Bucket::ALL.map(Bucket::minutes),
        [1, DEFAULT_MINUTES, 60, 240]
    );
    let bucket: Bucket = serde_json::from_value(json!("hour")).unwrap();
    assert_eq!(bucket.as_str(), "hour");
    assert_eq!(
        pass_time(&json!({"minutes": 0, "reason": "a blink"})),
        ToolOutput::Ok(json!({"minutes": 0, "takes": "0 min"}).to_string())
    );
    assert!(matches!(
        pass_time(&json!({"minutes": MAX_ADVANCE, "reason": "a week asleep"})),
        ToolOutput::Ok(_)
    ));
    for bad in [
        json!({"minutes": -1, "reason": "r"}),
        json!({"minutes": MAX_ADVANCE + 1, "reason": "r"}),
        json!({"minutes": 5}),
    ] {
        assert!(matches!(pass_time(&bad), ToolOutput::Err(_)), "{bad}");
    }
}
