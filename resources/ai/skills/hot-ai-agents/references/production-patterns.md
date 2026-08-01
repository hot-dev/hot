# Production Agent Patterns

## Contents

- [Choose the workflow topology](#choose-the-workflow-topology)
- [Carry causality in payloads](#carry-causality-in-payloads)
- [Design for at-least-once delivery](#design-for-at-least-once-delivery)
- [Gate effects](#gate-effects)
- [Preserve evidence and failures](#preserve-evidence-and-failures)
- [Separate canonical state from presentation](#separate-canonical-state-from-presentation)
- [Control context and providers](#control-context-and-providers)
- [Test offline first](#test-offline-first)

## Choose the Workflow Topology

Use one handler for a short atomic operation. Split a workflow when a stage
must be independently retryable, pausable, inspectable, or resumable.

For an asynchronous media pipeline:

- ingest the artifact;
- enqueue analysis with a literal event;
- record a durable job state;
- emit progress;
- perform provider work in a background handler;
- persist a normalized observation;
- emit structured observation and review events.

For a long-running reasoning workflow:

- represent each stage as its own handler;
- declare outgoing events in `sends` metadata;
- keep the domain loop independent of providers through typed ports;
- stop at a handler boundary for human approval;
- reconstruct state from an append-only fact log.

Literal event names make the topology visible to Hot's agent graph and to
reviewers. Avoid hiding the entire workflow behind dynamic dispatch.

## Carry Causality in Payloads

An event id identifies one delivery, not the entire causal chain. Carry stable
workflow identifiers in every payload:

```hot
{
    session_id,
    command_id,
    parent_ids,
    stage,
    stream_id,
}
```

Use the same command or workflow id across retries. Generate deterministic
stage and tool-call ids from stable inputs. Include parent fact ids when state
is append-only.

For a multi-stage handler:

```hot
next-data {
    ...event.data,
    command_id: command-id,
    parent_ids: concat(or(event.data.parent_ids, []), [recorded.id]),
}
send("agent:next-stage", next-data)
```

## Design for At-Least-Once Delivery

Assume handlers can be redelivered and events can arrive out of order.

Before expensive or effectful work:

1. derive a stable idempotency key;
2. check whether the stage already completed;
3. reserve or append atomically;
4. only the reservation winner performs the operation;
5. persist the result under the same key;
6. on redelivery, return or re-emit the stored result.

Never invoke a model twice merely because a queue redelivered a completed
stage. A second stochastic response under the same logical id corrupts audit
history.

Use `put-if-missing` or an append primitive for reservations. Treat a
reservation without a result as a recoverable state with an explicit timeout
or retry policy.

## Gate Effects

Classify every tool call as:

- unconditionally read-only;
- unconditionally effectful;
- dependent on the call input.

Perform classification on the concrete input, not only on the function name.
A shared SQL or HTTP tool can read or write depending on its arguments.

Default to effectful when:

- no classifier exists;
- the classifier fails;
- the classifier returns an invalid value;
- the verb or operation is unknown.

Before asking for approval:

1. redact secret-looking fields;
2. bound the preview;
3. explain the effect and why it is needed;
4. record the request as canonical state.

After approval, resume with the original command and idempotency key. Do not
let a second user event manufacture a new identity for the same effect.

## Preserve Evidence and Failures

Normalize results for routine reasoning while retaining evidence needed for
audit and debugging:

- provider-reported model version;
- prompt/resource provenance;
- token usage when known;
- raw response or a durable reference;
- normalized result;
- validation errors;
- tool inputs after redaction;
- tool outputs or artifact references.

Store oversized results as artifacts. Put a bounded head-and-tail projection
in model context so final error lines survive without flooding the prompt.

Do not fabricate a usable model result after a provider error. Record failure
as a fact and stop, or take a separately named deterministic fallback path.

## Separate Canonical State from Presentation

Use durable storage as the source of truth. Streams are a low-latency delivery
channel.

- Persist observations, facts, jobs, or log moves.
- Emit reply deltas for live prose.
- Emit structured state snapshots after meaningful transitions.
- Rebuild a resumed UI from storage or a fresh state snapshot.
- Do not expect historical `stream:data` frames to replay after reconnect.

For complex agents, a transcript is insufficient. State such as approvals,
ledger entries, current stage, sources, or analysis status needs its own
typed projection.

## Control Context and Providers

Keep provider calls behind typed application ports when prompts or response
shapes differ. A narrow port should accept only the context the role needs.

Examples:

- give a proposer broad state and all known constraints;
- give an independent critic the artifact and question, but not the
  proposer's reasoning;
- give a one-shot classifier one bounded artifact window and a JSON schema.

Resolve models and secrets from Hot context. Lock a model profile for a
long-lived experiment or session when reproducibility matters. Record the
provider-reported model, not only the requested alias.

Require explicit deployment opt-in before live model spending when an offline
fixture mode is supported.

## Test Offline First

Define deterministic fixtures behind the same typed ports as live providers.
Test:

- successful normalized output;
- malformed and truncated provider output;
- provider failure;
- tool failure returned to the model;
- maximum tool-loop iterations;
- duplicate event delivery;
- partial stage completion;
- approval grant and denial;
- out-of-order events;
- missing stream context;
- stream reconnect and fresh state emission;
- fallback behavior with no provider credentials.

Keep credentialed provider tests separate from the default suite.
