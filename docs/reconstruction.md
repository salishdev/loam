# Reconstruct knowledge from historical narration

This one-off example reads a **closed schema-6 save**, makes a new copy, and asks
Sonnet to extract explicitly narrated discoveries. It uses the live knowledge
writer, so the result opens normally and its learned entries survive replay and
forks. It adds no importer to the app and no extraction calls to gameplay.

```sh
cargo run --example reconstruct_knowledge -- --help
# No credentials or network: creates a supported fixture and imports its history.
cargo run --example reconstruct_knowledge -- --fixture /tmp/history.db /tmp/remembered.db /tmp/evidence.txt
# Real save (ANTHROPIC_API_KEY must already be exported; this example does not load .env):
cargo run --example reconstruct_knowledge -- old.db remembered.db evidence.txt
# Optional maximum narration characters per model request (default 12000; 1..48000):
cargo run --example reconstruct_knowledge -- old.db remembered.db evidence.txt 6000
cargo run -- --plain remembered.db
# In the game: :compendium player, :compendium player Bridge, :compendium world
```

Choose fresh paths each time. The example refuses an existing destination or
report, unsupported versions (including schema 5 rather than upgrading the source),
saves with SQLite `-wal`, `-shm` or `-journal` files, and inputs containing a prior
reconstruction marker. Close the game and leave it closed while copying; do not
copy a save that another process is writing. The source is read as bytes, never
opened for SQLite writes. Both successful and failed extractions leave it unchanged.

Only narration text and event numbers reach the model, along with previously
learned facts for identity and corrections. Player input is omitted: attempted
actions are not evidence of success. Hidden bible content, raw world properties,
off-screen ticks, tool results and judge reasoning never enter the request. Facts
need an exact excerpt of at least three words (at most 400 characters), a source
ID in that chunk, and the model's explicit support classification. Inferences and
ambiguities are flagged without importing them. A bad citation or invalid discovery
fails the run rather than silently dropping it. Quotes validate evidence exists;
they cannot prove a model's semantic interpretation is right. Review the report
against narration before trusting reconstructed memories.

Narration is processed chronologically in bounded chunks. Oversized events are
split with their original ID; an excerpt must fit within one passage. The request
includes a bounded 32000-character prior-entry context, prioritizing names mentioned
in the chunk, then recent entries. If older context is omitted, the model must flag
uncertain identity; duplicate names cannot silently merge. Corrections use existing
entry IDs and original fact numbers. Only learned facts available by that chunk
are included, never future summaries. Internal entity/rule/glossary IDs are not
established by ordinary prose, so reconstruction refuses those links instead of
guessing from matching names. Mechanics remain readable learned accounts; current
rule versions and conditions are not backfilled. Existing live entries use the same
writer's identity and fact deduplication checks.

The report starts `Status: INCOMPLETE`, records each imported discovery with entry ID,
source event, excerpt, attribution and supersession, and switches to `Status: COMPLETE` with a completion trailer only after
replay equals stored state. It includes ambiguity flags. An error appends `FAILED /
INCOMPLETE`; interruption leaves the initial incomplete status with no complete
trailer. A started reconstruction marker travels with replay and forks, preventing
repeat import even of partial runs. Do not play an incomplete output. Inspect it for
diagnosis, then delete **only the new destination, its SQLite sidecars if present,
and the new report** before retrying from the original, or choose fresh paths.
The script deliberately leaves artifacts rather than deleting evidence of failure.
It cannot resume a partial run. Original saves remain reusable to create independent
copies; it never silently overwrites or reimports into a reconstructed save.

The offline fixture spans three requests and demonstrates six entries: Mara's
cloak, the iron gate, pulling a lever, the bell, an attributed bridge rumor later
corrected by Mara, and snow on the road. Compare the report and printed details
against the quoted narration, then compare Player and World projections. The
fixture also contains hidden properties, tick/judge/bible secrets, an unfulfilled
player intention and ambiguous same-named people which must not become knowledge.
