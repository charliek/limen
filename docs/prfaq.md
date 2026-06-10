# PRFAQ — Safe, Accelerated Migration off Unsupported Frameworks

**A press-release-and-FAQ describing an approach — embodied in two open tools, a migration proxy and a functional test suite — for moving services off end-of-life frameworks and onto a supported paved path, without regressing behavior and without betting the migration on hope.**

*Audience: engineering leadership and senior engineers. This document is conceptual; it contains no configuration or code. It is company-neutral by design.*

---

## Part 1 — Press Release

### Teams can now migrate services off unsupported frameworks safely, with behavior verified at every step

Every engineering organization carries services built on frameworks that are no longer actively supported. As a framework reaches end of life, security patches stop, disclosed vulnerabilities go unaddressed, and the service's CVE exposure grows month over month through no change of its own. The code still runs; the risk underneath it keeps rising. Staying put is not stability — it is slow accumulation of unpatched attack surface.

The obvious response — rewrite the service on a supported, standardized framework (the "paved path") — is itself risky in the usual way it is done. A rewrite is a behavioral bet: the team reimplements the service, ships it, and hopes it behaves like the original. Subtle differences — a changed error shape, a dropped field, a timestamp format, an off-by-one in pagination — surface in production, as incidents, after the old implementation is gone. The fear of that outcome is precisely why high-risk-but-necessary migrations stall, leaving the CVE exposure in place.

This approach removes the bet. It pairs two tools that, together, let a team prove the new implementation matches the old one **before** real users depend on it, and shift traffic **gradually and reversibly** once it does:

- **A functional test suite** treats both the old and new services as black boxes and checks, deterministically, that for the same input they produce equivalent output. It runs locally and in CI, captures the old service's behavior as reviewable specifications, and turns "does the rewrite behave correctly?" into a test that either passes or fails.

- **A migration proxy** sits in front of both implementations in production. It can send a copy of real read traffic to the new service *without* affecting the user — the user is always served by the proven implementation — and compare the responses live, surfacing any divergence as a metric and a sampled diff. When divergence is acceptably low, the proxy shifts a controllable percentage of traffic to the new service, raises it in steps, and falls back to the old implementation automatically if anything goes wrong.

Connecting the two is a single **behavioral contract**: a reviewable description of what each endpoint should return and which incidental differences (request IDs, timestamps, key ordering) don't count. An engineer — assisted by AI — drafts the contract from the service's existing definition and traffic; the test suite validates and tightens it; the proxy then uses the *same, validated* contract to judge production traffic. The rules that prove correctness in testing are the rules that govern rollout.

**AI accelerates the laborious part without owning the decision.** Generating a thorough behavioral contract and a full set of comparison tests for a service is exactly the kind of high-volume, pattern-driven work that AI does quickly — reading an API definition and source, proposing which fields are dynamic noise versus meaningful behavior, and drafting success, error, and edge-case tests. A human reviews and approves that work before it gates anything or touches production. The result is migrations that move at the speed AI enables while remaining anchored to human judgment about what "correct" means.

The net effect: the organization can retire end-of-life frameworks **on a schedule it controls**, converting an open-ended, rising security risk into a sequence of verified, reversible steps — rather than postponing necessary migrations because the rewrite feels too dangerous to ship.

### Who this is for

Teams responsible for services on frameworks that are unsupported, deprecated, or accumulating CVE exposure, who need to move to a supported paved path but cannot afford behavioral regressions or risky big-bang cutovers. It applies to any HTTP/JSON service and any source-to-target framework or language change, because it validates behavior from the outside rather than depending on either technology's internals.

### What it deliberately is not

It is not a data-migration system: it assumes the new implementation reads and writes the same datastore as the old one, so the work is re-implementing logic, not moving data. It is not a load- or performance-testing suite, though it does verify the new service's latency and error rate stay within an agreed budget before traffic shifts. It does not replace a service's own unit tests. And it does not remove human review — the tools generate and recommend; people decide.

---

## Part 2 — Frequently Asked Questions

### Why not just stay on the current framework if the service works?

Because "works today" and "safe" are different properties. An unsupported framework stops receiving security fixes, so newly disclosed vulnerabilities in it — and in its transitive dependencies — remain unpatched indefinitely. The service's risk rises passively over time even if its code never changes. At some point that exposure becomes unacceptable (audit findings, compliance requirements, an actual incident), and the migration becomes mandatory under worse conditions than if it had been done deliberately. This approach exists to make the deliberate path tractable so migrations happen before they are forced.

### How is this different from a canary deploy or a feature flag?

A canary or flag shifts traffic between two versions, but it does not tell you whether the two versions *behave the same* — it just exposes some users to the new one and watches top-line health metrics. This approach adds the missing half: **response-level behavioral comparison**. Before any user is shifted, the proxy can shadow real traffic and compare the new service's actual responses against the old one's, field by field, surfacing differences a health metric would never catch (a subtly wrong value, a missing field, a changed error shape). The gradual traffic-shifting *is* flag-driven — but it is gated on proven behavioral parity, not just on "errors look fine so far."

### How is this different from contract testing or consumer-driven contracts?

Conventional contract testing checks a service against a contract that humans wrote up front, describing what the service *should* do. That is valuable but it presumes someone correctly specified the contract. Here, the "contract" is derived from the **legacy service's observed behavior** — the actual source of truth you must preserve — and then validated by comparing the two implementations directly. The goal is not "does the new service satisfy an idealized spec" but "does the new service behave like the thing it is replacing, including the behaviors clients already depend on." It is closer to characterization testing of the legacy service, applied as the acceptance bar for the rewrite.

### Doesn't shadowing real traffic double the load and cost on the new service?

For the read traffic that gets shadowed, yes — the new service receives a copy of those requests. This is a deliberate, bounded cost and the reason shadowing is configurable: teams sample a fraction of traffic rather than mirroring all of it, cap the size of what gets compared, and throttle shadow volume under load. The new service is being stood up to take this traffic eventually regardless, so shadowing is also a realistic warm-up under production conditions. Crucially, the shadow path never affects the user — the cost is borne by the new service and the proxy, not by client-visible latency.

### What happens if the AI writes a wrong test or a bad comparison rule?

This is the central risk, and the workflow is built around it. AI-generated tests and comparison rules are **proposals**, not authority. They pass through a mandatory human review gate before they are trusted as a correctness bar or consumed by the proxy, where a person checks that the tests encode behavior the organization actually wants preserved and that no comparison rule is hiding a real difference. Beyond that gate, the test suite itself catches a whole class of AI errors: an over-broad rule that ignores a meaningful field tends to surface as a test that passes when it shouldn't during the refinement loop, and a too-strict rule surfaces as a false failure the engineer investigates. The design assumption is that AI will sometimes be wrong, so correctness never rests on the AI being right — it rests on deterministic tests and human sign-off.

### Why two separate tools instead of one?

Because they do two different jobs in two different settings, and coupling them would weaken both. The test suite runs in development and CI, must be deterministic and fast, and is where engineers and reviewers live — its natural home is the testing ecosystem. The proxy runs in production, must be high-performance and fail-safe on the live request path, and is an operational component. Keeping them independent lets each be the right tool for its job and lets a team adopt one without the other (use the test suite alone for confidence, or the proxy alone for rollout control). They are connected by the shared behavioral contract, not by a code dependency, so the integration is a stable format rather than a brittle coupling.

### Why might the two tools be built in different technologies?

For the same reason they are separate: each is optimized for its job. A production proxy on the live traffic path benefits from a systems language with strong performance and resource guarantees. A functional test suite benefits from the ecosystem and iteration speed of the environment its tests and authors already inhabit. The shared contract is a plain, portable format that both read identically, so the technology choice on each side is an implementation detail, not a constraint on interoperability.

### What about writes? You keep saying "reads."

Reads and writes are handled differently on purpose. Reads have no side effects, so they can be shadowed and compared freely — this is the safe, high-volume part. Writes are never shadowed by default, because executing a write through both implementations against a shared datastore would apply the side effect twice. Instead, writes are validated by driving the new implementation against controlled test data and asserting the result — including reading the written resource back through an already-proven read path and confirming it matches what the old implementation would have produced. In production, write traffic is shifted to exactly one implementation at a time (never both), and the proxy will not blindly retry a write that may have already taken effect. Writes move later in the sequence and with more scrutiny than reads.

### Isn't the shared-datastore assumption limiting?

It is a deliberate scoping choice that matches the most common and most valuable case: re-implementing a service's logic on a supported framework while it continues to use the same database. Under that assumption, correctness reduces cleanly to behavioral parity, and the approach is sound and simple. Migrations that also move or split the datastore are a genuinely harder problem — they require data synchronization and reconciliation that this approach intentionally does not attempt. Scoping to the shared-store case is what lets the tools make strong safety guarantees rather than weak ones across every possible migration.

### How do we know the proxy itself isn't the problem — adding latency or hiding errors?

The proxy is treated as a component that must prove its own health, separately from the service. It is held to an explicit overhead budget on the traffic path, and its behavior is observable: how much traffic is actually being compared, whether comparison is being skipped, the state of its automatic fallback mechanisms, the health of its rollout controls. The approach is explicit that a fast proxy in front of a slow new service is not a green light, and that a misbehaving proxy can make a healthy service look bad — so both the proxy's overhead and the new service's performance are checked independently, and both must be within budget before traffic advances.

### What does "safe to roll out" actually mean — when do we advance?

Advancement is gated on a defined budget, not a feeling. Before traffic shifts, the team sets thresholds for three things: behavioral parity (how often the new service's responses differ from the old one's, and zero unexplained differences in high-risk fields), error rate (the new service must be no worse than the old), and latency (the new service must meet or beat the old, or carry a documented, bounded exception). Traffic advances through small percentage steps only while those thresholds hold, and any breach lowers the percentage or trips automatic fallback. Every recurring difference is either fixed or explicitly acknowledged as an intended change before rollout continues.

### How fast can a service actually be migrated this way?

It varies with the service's complexity and risk, and the approach intentionally trades a little up-front speed for a lot of downside protection. What it compresses is the most time-consuming work — characterizing the old service's behavior and building a thorough comparison suite — by having AI draft it from the existing API definition, source, and traffic, with a human reviewing. What it deliberately does not compress is the verification: the gradual, gated rollout takes the time it takes to observe real traffic at each step. The point is not to make migrations instantaneous; it is to make them *safe enough to actually start*, so that necessary work stops being deferred.

### What if we discover the old service has bugs we don't want to keep?

The approach surfaces them rather than blindly preserving them. When the two implementations differ, the difference is reviewed, and the team decides whether it is a behavior to preserve or a bug to fix. Intended deviations are marked explicitly as intentional changes so reviewers and the rollout process treat them as expected rather than as regressions. The migration thus becomes an opportunity to consciously decide which legacy behaviors carry forward — instead of either accidentally preserving every quirk or accidentally breaking a behavior a client depended on.

### Where does this leave us once a migration is done?

The behavioral test suite does not get thrown away at cutover — it becomes the new service's regression suite, continuing to run in CI and guarding against future drift. The validated contract remains the reference for what correct behavior looks like. And the same approach, tooling, and reviewed patterns apply to the next service, so each migration makes the organization faster and more confident at the next one. Over a portfolio of services on aging frameworks, that compounding is the real payoff: a repeatable, lower-risk path to retiring unsupported technology on the organization's own schedule.

---

## Appendix — One-paragraph summary for a busy reader

Services on end-of-life frameworks accumulate unpatched CVE exposure over time, but rewriting them onto a supported paved path is usually a behavioral gamble that teams postpone because it feels too risky to ship. This approach removes the gamble: a functional test suite proves the new implementation behaves like the old one before users depend on it, and a migration proxy shadows real read traffic to verify parity in production and then shifts traffic gradually and reversibly, falling back automatically if anything regresses. A single human-reviewed behavioral contract — drafted with AI assistance, validated by the tests, and enforced by the proxy — ties the two together. The result is a repeatable, gated, reversible way to retire unsupported frameworks on a controlled schedule, with AI accelerating the work and human judgment owning the definition of correct.
