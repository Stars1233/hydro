# The Life and Times of a Hydroflow Spinner
Time is a fundamental concept in many distributed systems. Hydroflow's model of time is very simple.

Like most reactive services, we can envision a Hydroflow spinner running as an unbounded loop that is managed 
by the runtime library. Each iteration of the spinner's loop is called a *tick*. Associated with the spinner is 
a *clock* value (accessible via the `.current_tick()` method), which tells you how many ticks were executed 
by this spinner prior to the current tick. Each spinner produces totally-ordered, sequentially increasing clock values, 
which you can think of as the "local logical time" at the spinner.

The spinner's main loop works as follows:
1. Given events and messages buffered from the operating system, ingest a batch of data items and deliver them to the appropriate `source_xxx` operators in the Hydroflow spec.
2. Run the Hydroflow spec. If the spec has cycles, continue executing it until it reaches a "fixpoint" on the current batch; i.e. it no longer produces any new data anywhere in the flow. Along the way, any data that appears in an outbound channel is streamed to the appropriate destination.
3. Advance the local clock before starting the next tick.

The spinner's main loop is shown in the following diagram:

```mermaid
%%{init: {'theme':'neutral'}}%%
flowchart LR
    network>events and messages fa:fa-telegram]--->buffer[[buffer]]--->ingest>ingest a batch of data]--->loop(((Run Hydroflow Spec to Fixpoint fa:fa-cog)))--->stream[stream outputs fa:fa-telegram]--->clock((advance clock fa:fa-clock-o))--->ingest
    style stream fill:#0fa,stroke:#aaa,stroke-width:2px,stroke-dasharray: 5 5
    style loop fill:#0fa
    style clock fill:#f00
    style ingest fill:#f00
```

In sum, an individual spinner advances sequentially through logical time; in each tick of its clock it ingests a batch of data from its inbound channels, executes the Hydroflow spec, and sends any outbound data to its outbound channels.