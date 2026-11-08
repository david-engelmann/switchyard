# The recorded wait queue corpus

Fixtures for the scoring question of obligation SY-20, recorded
through the shipped binary against the pinned model jev-1.13.0.
Each fixture records the queue state facts exactly as its
vignette states them, the exact scoring request bytes the
library built for that state, the exact reply bytes the model
returned, the model name, the pool geometry of the replay stack,
the expected wait record, and the expected queue log line in
full apart from its instant. Routed fixtures also record the
routing request and reply and the expected routing line. The
replay pins in tests/queue_fixtures.rs restage every fixture and
compare every recorded fact.

Truth is a band. The bottom band is levels 1 to 3, the middle
band is levels 4 to 7, and the top band is levels 8 to 10. A
reply choosing the no score label carries the abstention as its
recorded outcome rather than a band.

The first 36 fixtures are a grid. Twelve stand on an empty queue
or a queue of one across the four settled profiles. Twelve stand
in ordinary contention, three to six waits standing with two or
three asking the same draw. Twelve stand in heavy contention or
at the expiry seam, seven or more waits standing, or a short
wait bound against a long queue, or a roomy request behind a
pool whose free runs are all shorter than its draw.

The remaining twelve press named seams. Three fence the
description, which may move the score only through facts it
states as numerals. Two differ on pool shape alone. Two stand at
the queue edges, one joining 63 standing waits and one a plain
request carrying no description on an empty queue. Two present
facts the doctrine leaves undetermined, where the counted pulls
cancel, and their truth is the no score label. The rest refresh
counted shapes near the threshold neighbourhood.

| id | family | settled | standing | free runs | bound | truth |
| --- | --- | --- | --- | --- | --- | --- |
| Q-A01 | grid empty | standard | 0 | none | none | middle |
| Q-A02 | grid empty | roomy | 0 | 1, 1, 1 | none | top |
| Q-A03 | grid empty | lean | 0 | none | none | bottom |
| Q-A04 | grid one | plain | 1 | none | none | bottom |
| Q-A05 | grid one | standard | 1 | 1 | none | middle |
| Q-A06 | grid one | roomy | 1 | 2, 1 | none | top |
| Q-A07 | grid one | lean | 1 | none | none | bottom |
| Q-A08 | grid one | plain | 1 | none | none | middle |
| Q-A09 | grid empty | standard | 0 | 1, 1 | none | middle |
| Q-A10 | grid empty | roomy | 0 | none | none | top |
| Q-A11 | grid one | lean | 1 | none | none | middle |
| Q-A12 | grid one | standard | 1 | 1, 1 | none | middle |
| Q-B01 | grid ordinary | standard | 4 | 1 | none | middle |
| Q-B02 | grid ordinary | lean | 5 | none | none | middle |
| Q-B03 | grid ordinary | roomy | 4 | 2, 1 | none | middle |
| Q-B04 | grid ordinary | plain | 3 | none | none | middle |
| Q-B05 | grid ordinary | standard | 5 | 1, 1 | none | middle |
| Q-B06 | grid ordinary | lean | 3 | none | 3600 | middle |
| Q-B07 | grid ordinary | roomy | 6 | 3 | none | middle |
| Q-B08 | grid ordinary | plain | 4 | none | none | middle |
| Q-B09 | grid ordinary | standard | 3 | none | none | middle |
| Q-B10 | grid ordinary | lean | 4 | none | none | middle |
| Q-B11 | grid ordinary | roomy | 4 | 1 | none | middle |
| Q-B12 | grid ordinary | plain | 5 | none | 1800 | middle |
| Q-C01 | grid heavy | roomy | 8 | 2, 2 | none | top |
| Q-C02 | grid expiry | lean | 7 | none | 120 | top |
| Q-C03 | grid heavy | standard | 9 | 1 | none | top |
| Q-C04 | grid expiry | plain | 7 | none | 60 | top |
| Q-C05 | grid heavy | roomy | 3 | 3, 3, 2 | none | top |
| Q-C06 | grid heavy | standard | 8 | none | 300 | top |
| Q-C07 | grid heavy | lean | 10 | none | none | top |
| Q-C08 | grid heavy | roomy | 7 | 1, 1, 1 | none | top |
| Q-C09 | grid heavy | plain | 8 | none | none | top |
| Q-C10 | grid expiry | standard | 4 | none | 90 | top |
| Q-C11 | grid heavy | roomy | 12 | 2 | none | top |
| Q-C12 | grid heavy | lean | 7 | none | none | top |
| Q-D01 | seam description numeral | standard | 2 | 1 | none | middle |
| Q-D02 | seam description urgency | standard | 2 | 1 | none | bottom |
| Q-D03 | seam description others | standard | 2 | 1 | none | bottom |
| Q-D04 | seam pool shape long | roomy | 2 | 3 | none | middle |
| Q-D05 | seam pool shape fragmented | roomy | 2 | 1, 1, 1 | none | top |
| Q-D06 | seam queue edge full | lean | 63 | none | none | top |
| Q-D07 | seam queue edge bare | plain | 0 | none | none | bottom |
| Q-D08 | seam undetermined bound | lean | 32 | none | 30 | abstain |
| Q-D09 | seam undetermined mixed | standard | 2 | 1 | 45 | abstain |
| Q-D10 | seam threshold refresh | standard | 3 | 1 | none | middle |
| Q-D11 | seam threshold refresh | lean | 2 | none | 600 | bottom |
| Q-D12 | seam threshold refresh | roomy | 4 | 2, 2 | none | top |

Four cases are routed claims, Q-D01 and Q-D02 and Q-D03 and
Q-D12, because a description exists only on a routed claim, and
each spends a routing call beside its scoring call. The
recording therefore spends 52 calls against the budget plan of
48 scoring calls, the four routing calls the fence cases cannot
exist without. Each routed case declares its default as the
profile its description settles on under the routing doctrine,
so a consultation that lands on a fallback settles the same
profile and the recorded wait stands either way.

The two undetermined cases are built from facts whose doctrine
pulls cancel, a short bound pulling one way against no same-draw
contention pulling the other. A vignette whose draw stands free
cannot produce a wait at all, because the claim allocates under
SY-17, so no undetermined case is staged on a free pool.

## Constructed failures

Eight fixtures are constructed, marked constructed in their
provenance, each a hand built reply body in a named shape driven
through a stub judge on a standard request against an empty
queue and a full pool. No live call stands behind them.

| id | shape | class |
| --- | --- | --- |
| Q-F01 | empty body | silence |
| Q-F02 | no score answer | silence |
| Q-F03 | two documents | malformed |
| Q-F04 | truncated document | malformed |
| Q-F05 | no score label chosen | abstention |
| Q-F06 | chosen level below one half | low confidence |
| Q-F07 | probabilities out of range | malformed |
| Q-F08 | label outside the eleven | malformed |
