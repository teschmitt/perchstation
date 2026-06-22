## ADDED Requirements

### Requirement: Cross-process capture status projection

A separate-process `perchstation status` SHALL report the running capture loop's latest observed state rather than the "never observed" default.

The reported state — last recording time and clip id, last capture failure (kind
and message), and current sensor liveness — SHALL reflect changes in the running
capture loop within the SC-007 budget of 30 seconds. When `status` is computed
in the same process as the capture loop (an in-process `CaptureState` is
available, as in integration tests), that in-process state SHALL remain the
source of truth and behavior SHALL be unchanged.

#### Scenario: Separate-process status reflects a recent recording

- **WHEN** `serve` is running, has recorded at least one clip, and an operator
  runs `perchstation status` as a separate process against the same `data_dir`
- **THEN** the capture block shows the last recording time and clip id from the
  running capture loop, not `(none)`

#### Scenario: Separate-process status reflects a degraded sensor within budget

- **WHEN** the running capture loop marks the sensor `stuck_asserted` or
  `unavailable`
- **THEN** a `perchstation status` invocation made at least 30 seconds later, as
  a separate process, reports that degraded liveness (with its degraded-since
  time), not `(never observed)` or `healthy`

#### Scenario: In-process status is unchanged

- **WHEN** `status` is computed with a live in-process `CaptureState` supplied
- **THEN** the capture block is taken from that in-process state and the
  persisted projection is not consulted

### Requirement: Projection freshness and fallback

The persisted capture projection SHALL record the time it was last written
(`as_of`). `perchstation status` SHALL distinguish a fresh projection from a
stale one and SHALL NOT present a stale projection as a live reading. When no
projection exists, `status` SHALL fall back to the "never observed" default.

A projection whose `as_of` is older than the staleness threshold (a small
multiple of the projection's refresh cadence) SHALL be rendered as stale,
annotated with its `as_of` time, so that a sensor last seen `healthy` by a
`serve` that has since stopped is not reported as currently `healthy`.

#### Scenario: No projection falls back to never observed

- **WHEN** no capture projection file exists under `data_dir` (e.g. `serve` has
  never run)
- **THEN** `perchstation status` reports the capture block as `(never observed)`
  / `(none)`, exactly as today

#### Scenario: Stale projection is marked stale, not live

- **WHEN** a capture projection exists but its `as_of` is older than the
  staleness threshold (the writing `serve` has stopped)
- **THEN** `perchstation status` renders the capture block annotated with its
  `as_of` time and does not assert the sensor is currently `healthy`

#### Scenario: Fresh projection is reported as current

- **WHEN** a capture projection exists and its `as_of` is within the staleness
  threshold
- **THEN** `perchstation status` reports its values as the current capture state

### Requirement: Read/write safety of the projection

Writing the capture projection SHALL be atomic, so a concurrent
`perchstation status` read never observes a partial or corrupt projection.
Reading the projection SHALL remain a pure, side-effect-free operation that does
not mutate anything under `data_dir`, preserving the guarantee that `status` is
safe to run alongside `serve`.

A projection file that is missing or unparseable SHALL be treated as "no
projection" (fall back to the never-observed default) and SHALL NOT cause
`perchstation status` to fail.

#### Scenario: Concurrent read never sees a torn write

- **WHEN** `serve` refreshes the projection while `perchstation status` reads it
- **THEN** `status` observes either the previous complete projection or the new
  complete projection, never a partial file

#### Scenario: Corrupt projection degrades gracefully

- **WHEN** the projection file is present but unparseable
- **THEN** `perchstation status` falls back to the never-observed default and
  exits successfully rather than erroring
