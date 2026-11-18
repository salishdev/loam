//! A game's branches: the saves in one directory that share a lineage.
//!
//! Every save knows its `lineage` (shared by all branches of one game), the
//! `parent` save it was forked from and the event it was `forked_at` (see
//! [`World::fork`](crate::world::World::fork)). [`scan`] finds the current save's relatives next to it,
//! in tree order; [`tree`] draws them for `:branches`; [`resolve`] reads a
//! `:switch` argument; [`next_path`] names a new branch. Nothing here calls
//! a model.

use std::path::{Path, PathBuf};

use crate::world::StoreError;

/// One save of the current game, as [`scan`] found it.
#[derive(Debug, Clone, PartialEq)]
pub struct Branch {
    /// The save, canonicalized.
    pub path: PathBuf,
    /// The save it was forked from, when that is in the same directory.
    pub parent: Option<PathBuf>,
    /// The event it was forked at, if it is a branch.
    pub forked_at: Option<i64>,
    pub label: Option<String>,
    /// The last event's sequence number (0 for an empty log).
    pub last_seq: i64,
    /// When the last event was written (RFC 3339, UTC).
    pub last_at: Option<String>,
    /// How deep in the tree: 0 for a root.
    pub depth: usize,
    /// This is the save being played.
    pub current: bool,
}

impl Branch {
    /// The save's file name.
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    }
}

/// The saves in `current`'s directory with `current`'s lineage (including
/// it), in tree order: each save followed by the branches forked from it,
/// those ordered by fork point, then name. Files that aren't loam saves,
/// saves too old to have a lineage and other games' saves are skipped.
pub fn scan(current: &Path) -> Result<Vec<Branch>, StoreError> {
    let current = canonical(current);
    let lineage = peek(&current)?
        .lineage
        .ok_or_else(|| StoreError::Corrupt("meta lineage is missing".into()))?;
    let dir = current
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let mut found = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| StoreError::Corrupt(format!("can't list {}: {e}", dir.display())))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "db") || !path.is_file() {
            continue;
        }
        let path = canonical(&path);
        // Anything that won't read as a loam save isn't a branch.
        let Ok(save) = peek(&path) else {
            continue;
        };
        if save.lineage.as_deref() != Some(lineage.as_str()) {
            continue;
        }
        found.push(Branch {
            parent: save.parent.map(|p| canonical(Path::new(&p))),
            forked_at: save.forked_at,
            label: save.label,
            last_seq: save.last_seq,
            last_at: save.last_at,
            depth: 0,
            current: path == current,
            path,
        });
    }
    Ok(order(found))
}

/// What [`scan`] reads from one save.
struct Peeked {
    lineage: Option<String>,
    parent: Option<String>,
    forked_at: Option<i64>,
    label: Option<String>,
    last_seq: i64,
    last_at: Option<String>,
}

/// Reads a save's branch facts without changing it: no create, no schema
/// upgrade, no writes (`query_only`). It opens read-write all the same,
/// because a read-only connection to a WAL save no one else has open
/// leaves empty `-wal` and `-shm` files behind; a read-write one, closing
/// last, removes them.
fn peek(path: &Path) -> Result<Peeked, StoreError> {
    use rusqlite::{Connection, OpenFlags, OptionalExtension};
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA query_only = ON")?;
    let meta = |key: &str| -> Result<Option<String>, StoreError> {
        Ok(conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    };
    let forked_at = match meta("forked_at")? {
        Some(v) => Some(
            v.parse()
                .map_err(|_| StoreError::Corrupt(format!("meta forked_at = {v:?}")))?,
        ),
        None => None,
    };
    let last: Option<(i64, String)> = conn
        .query_row(
            "SELECT seq, at FROM events ORDER BY seq DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(Peeked {
        lineage: meta("lineage")?,
        parent: meta("parent")?,
        forked_at,
        label: meta("label")?,
        last_seq: last.as_ref().map_or(0, |(seq, _)| *seq),
        last_at: last.map(|(_, at)| at),
    })
}

/// Puts `branches` in tree order and sets their depths. A save whose
/// parent isn't among them is a root.
fn order(mut branches: Vec<Branch>) -> Vec<Branch> {
    branches.sort_by(|a, b| {
        (a.forked_at.unwrap_or(-1), a.name()).cmp(&(b.forked_at.unwrap_or(-1), b.name()))
    });
    let paths: Vec<PathBuf> = branches.iter().map(|b| b.path.clone()).collect();
    let is_root = |b: &Branch| {
        b.parent
            .as_ref()
            .is_none_or(|p| !paths.contains(p) || *p == b.path)
    };
    let mut out = Vec::with_capacity(branches.len());
    let mut taken = vec![false; branches.len()];
    // Depth-first from each root; `taken` guards against parent cycles.
    let mut stack: Vec<(usize, usize)> = branches
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, b)| is_root(b))
        .map(|(i, _)| (i, 0))
        .collect();
    loop {
        while let Some((i, depth)) = stack.pop() {
            if taken[i] {
                continue;
            }
            taken[i] = true;
            let mut b = branches[i].clone();
            b.depth = depth;
            let me = b.path.clone();
            out.push(b);
            for (j, child) in branches.iter().enumerate().rev() {
                if !taken[j] && child.parent.as_ref() == Some(&me) && child.path != me {
                    stack.push((j, depth + 1));
                }
            }
        }
        // Saves caught in a parent cycle have no root; show them as roots.
        match taken.iter().position(|t| !t) {
            Some(i) => stack.push((i, 0)),
            None => break,
        }
    }
    out
}

/// The tree as text, one numbered line per save; `*` marks the current
/// one. For example:
///
/// ```text
///   1  mill.db · #142 · 2026-11-18 09:10
/// * 2  ├─ mill.b1.db “kicked the door” · from #80 · #95 · 2026-11-18 10:02
///   3  └─ mill.b2.db · from #120 · #120 · 2026-11-18 10:30
/// ```
pub fn tree(branches: &[Branch]) -> String {
    let mut lines = Vec::with_capacity(branches.len());
    for (i, b) in branches.iter().enumerate() {
        let mut prefix = String::new();
        for level in 1..=b.depth {
            let last = is_last_at(branches, i, level);
            prefix.push_str(match (level == b.depth, last) {
                (true, true) => "└─ ",
                (true, false) => "├─ ",
                (false, true) => "   ",
                (false, false) => "│  ",
            });
        }
        let mut line = format!(
            "{} {}  {prefix}{}",
            if b.current { '*' } else { ' ' },
            i + 1,
            b.name()
        );
        if let Some(label) = &b.label {
            line.push_str(&format!(" “{label}”"));
        }
        if let Some(at) = b.forked_at {
            line.push_str(&format!(" · from #{at}"));
        }
        line.push_str(&format!(" · #{}", b.last_seq));
        if let Some(at) = &b.last_at {
            line.push_str(&format!(" · {}", short_time(at)));
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Whether row `i`'s ancestor at `level` (the row itself at its own depth)
/// is the last child of its parent: no later row at that depth before the
/// subtree ends.
fn is_last_at(branches: &[Branch], i: usize, level: usize) -> bool {
    !branches[i + 1..]
        .iter()
        .take_while(|b| b.depth >= level)
        .any(|b| b.depth == level)
}

/// `2026-11-18T10:02:33.123Z` → `2026-11-18 10:02`.
fn short_time(at: &str) -> String {
    match (at.get(..10), at.get(11..16)) {
        (Some(day), Some(time)) => format!("{day} {time}"),
        _ => at.to_string(),
    }
}

/// The branch a `:switch` argument names: a row number from [`tree`], a
/// file name with or without `.db`, or a label (any case).
pub fn resolve<'a>(branches: &'a [Branch], arg: &str) -> Option<&'a Branch> {
    let arg = arg.trim();
    if arg.is_empty() {
        return None;
    }
    if let Ok(n) = arg.trim_start_matches('#').parse::<usize>() {
        return n.checked_sub(1).and_then(|i| branches.get(i));
    }
    let file = Path::new(arg)
        .file_name()
        .map_or_else(|| arg.to_string(), |f| f.to_string_lossy().into_owned());
    branches
        .iter()
        .find(|b| {
            let name = b.name();
            name == file || name.strip_suffix(".db") == Some(file.as_str())
        })
        .or_else(|| {
            branches.iter().find(|b| {
                b.label
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case(arg))
            })
        })
}

/// Where a new branch of `current` goes when no path is given:
/// `<stem>.b<N>.db` beside it, with the next free `N`. A `.b<N>` already at
/// the end of the stem is dropped first, so branches of branches keep flat
/// names (`mill.b1.db` → `mill.b2.db`); the tree shows who forked from whom.
pub fn next_path(current: &Path) -> PathBuf {
    let dir = current.parent().unwrap_or(Path::new(""));
    let stem = current
        .file_stem()
        .map_or_else(|| "loam".to_string(), |s| s.to_string_lossy().into_owned());
    let stem = match stem.rsplit_once(".b") {
        Some((base, n))
            if !base.is_empty() && !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()) =>
        {
            base.to_string()
        }
        _ => stem,
    };
    (1..)
        .map(|n| dir.join(format!("{stem}.b{n}.db")))
        .find(|p| !p.exists())
        .expect("some branch number is free")
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::game::tests::begun;
    use crate::world::World;

    /// A fresh temp directory for one test.
    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("loam-branches-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A begun game saved as `dir/name`; the in-memory original is dropped.
    pub(crate) fn saved_game(dir: &Path, name: &str) -> PathBuf {
        let (game, _) = begun();
        let path = dir.join(name);
        let last = game.world().last_seq().unwrap();
        drop(game.world().fork(last, &path).unwrap());
        // A plain save, not a branch of the in-memory world.
        strip_branch_meta(&path);
        path
    }

    fn strip_branch_meta(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute("DELETE FROM meta WHERE key IN ('forked_at', 'parent')", [])
            .unwrap();
    }

    fn names(branches: &[Branch]) -> Vec<(String, usize, bool)> {
        branches
            .iter()
            .map(|b| (b.name(), b.depth, b.current))
            .collect()
    }

    #[test]
    fn scan_finds_the_lineage_and_ignores_unrelated_saves() {
        let dir = temp_dir("scan");
        let root = saved_game(&dir, "mill.db");
        let world = World::open(&root).unwrap();
        let b1 = world.fork(3, dir.join("mill.b1.db")).unwrap();
        b1.set_label("early").unwrap();
        let b2 = world
            .fork(world.last_seq().unwrap(), dir.join("mill.b2.db"))
            .unwrap();
        // A branch of a branch.
        drop(b1.fork(2, dir.join("deep.db")).unwrap());
        drop((b1, b2));
        // Another game, a non-SQLite .db, a text file.
        saved_game(&dir, "other.db");
        std::fs::write(dir.join("junk.db"), "not a database").unwrap();
        std::fs::write(dir.join("notes.txt"), "hi").unwrap();
        drop(world);

        let branches = scan(&dir.join("mill.b1.db")).unwrap();
        assert_eq!(
            names(&branches),
            [
                ("mill.db".into(), 0, false),
                ("mill.b1.db".into(), 1, true),
                ("deep.db".into(), 2, false),
                ("mill.b2.db".into(), 1, false),
            ]
        );
        assert_eq!(branches[1].label.as_deref(), Some("early"));
        assert_eq!(branches[1].forked_at, Some(3));
        assert_eq!(branches[1].last_seq, 3);
        assert!(branches[1].last_at.is_some());
        assert_eq!(branches[0].forked_at, None);
        // The other game sees only itself.
        let other = scan(&dir.join("other.db")).unwrap();
        assert_eq!(names(&other), [("other.db".into(), 0, true)]);
        // Scanning saves no one has open leaves no WAL files behind.
        let stray: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with("-wal") || n.ends_with("-shm"))
            .collect();
        assert!(stray.is_empty(), "{stray:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn branch(name: &str, depth: usize) -> Branch {
        Branch {
            path: PathBuf::from(format!("/saves/{name}")),
            parent: None,
            forked_at: (depth > 0).then_some(10),
            label: None,
            last_seq: 20,
            last_at: Some("2026-11-18T10:02:33.123Z".into()),
            depth,
            current: false,
        }
    }

    #[test]
    fn tree_draws_nesting_labels_and_the_current_save() {
        let mut rows = vec![
            branch("a.db", 0),
            branch("b.db", 1),
            branch("c.db", 2),
            branch("d.db", 1),
            branch("e.db", 2),
        ];
        rows[1].label = Some("kicked the door".into());
        rows[3].current = true;
        rows[0].last_at = None;
        assert_eq!(
            tree(&rows),
            "  1  a.db · #20\n\
             \x20 2  ├─ b.db “kicked the door” · from #10 · #20 · 2026-11-18 10:02\n\
             \x20 3  │  └─ c.db · from #10 · #20 · 2026-11-18 10:02\n\
             * 4  └─ d.db · from #10 · #20 · 2026-11-18 10:02\n\
             \x20 5     └─ e.db · from #10 · #20 · 2026-11-18 10:02"
        );
    }

    #[test]
    fn order_puts_children_under_parents_by_fork_point() {
        let mut a = branch("a.db", 0);
        a.forked_at = None;
        let mut late = branch("late.db", 0);
        late.parent = Some(a.path.clone());
        late.forked_at = Some(50);
        let mut early = branch("zz-early.db", 0);
        early.parent = Some(a.path.clone());
        early.forked_at = Some(5);
        // Its parent isn't in the directory: a root.
        let mut orphan = branch("orphan.db", 0);
        orphan.parent = Some(PathBuf::from("/elsewhere/x.db"));
        let got = order(vec![late, orphan, a, early]);
        let got: Vec<(String, usize)> = got.iter().map(|b| (b.name(), b.depth)).collect();
        assert_eq!(
            got,
            [
                ("a.db".into(), 0),
                ("zz-early.db".into(), 1),
                ("late.db".into(), 1),
                ("orphan.db".into(), 0),
            ]
        );
    }

    #[test]
    fn resolve_takes_numbers_names_and_labels() {
        let mut rows = vec![branch("mill.db", 0), branch("mill.b1.db", 1)];
        rows[1].label = Some("Door Kicked".into());
        assert_eq!(resolve(&rows, "2").unwrap().name(), "mill.b1.db");
        assert_eq!(resolve(&rows, "mill").unwrap().name(), "mill.db");
        assert_eq!(resolve(&rows, "./mill.b1.db").unwrap().name(), "mill.b1.db");
        assert_eq!(resolve(&rows, "door kicked").unwrap().name(), "mill.b1.db");
        assert!(resolve(&rows, "3").is_none());
        assert!(resolve(&rows, "0").is_none());
        assert!(resolve(&rows, "nope").is_none());
    }

    #[test]
    fn next_path_numbers_branches_beside_the_save() {
        let dir = temp_dir("naming");
        let save = dir.join("mill.db");
        assert_eq!(next_path(&save), dir.join("mill.b1.db"));
        std::fs::write(dir.join("mill.b1.db"), "").unwrap();
        std::fs::write(dir.join("mill.b3.db"), "").unwrap();
        assert_eq!(next_path(&save), dir.join("mill.b2.db"));
        // A branch's branch keeps a flat name.
        assert_eq!(next_path(&dir.join("mill.b1.db")), dir.join("mill.b2.db"));
        // Only a numeric `.bN` is a branch suffix.
        assert_eq!(next_path(&dir.join("a.bx.db")), dir.join("a.bx.b1.db"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
