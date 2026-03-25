# The netfyr Governance Model

The netfyr project operates as an agentic-first "dark software factory." Because our goal is to minimize and eventually eliminate human code review, our governance relies on defining strict requirements, maintaining robust automated testing, and managing the AI agents that write the code.

## Project Roles

* **Stakeholders:** Users, sponsors, and community members who provide feedback on the prototypes and demos. They define whether the factory's output is actually useful and impactful.
* **Spec Contributors:** Community members who submit issues, requirements, test cases, and `nlspec` files. Their job is to tell the factory *what* to build and *how* to verify it.
* **Factory Operators (Maintainers):** Core team members responsible for maintaining the engine of the dark factory itself. 
    * They do *not* review code. 
    * They manage agent sandboxing, API budgets, and CI/CD pipelines.
    * They approve the underlying agent workflows (e.g., Attractor or RuFlo updates).
* **The Agents (Non-Human Actors):** Autonomous systems that parse `nlspecs` and generate code. They are treated as the sole "Committers" of source code in the repository.

## Decision-Making

Decisions about the project — architecture, priorities, spec approval, factory configuration, process changes — are made by **consent among the Senior Maintainers**. Consent means no Senior Maintainer has a reasoned objection; it does not require enthusiastic agreement from everyone, only the absence of blocking concerns.

When the Senior Maintainers cannot reach consent, Till Maas makes the final call. This is a tiebreaker, not the default — the expectation is that most decisions are resolved through discussion and consent.

### The Zero-Review Pipeline

Because we aim for a trusted dark factory, code merging is governed by automation, not human consensus:

1. **Spec Approval:** Human collaboration happens entirely at the specification layer. Senior Maintainers review `nlspecs` and architectural intents for clarity, safety, and alignment with the Linux host network management vision.
2. **Autonomous Execution:** Once a spec is approved, it is handed off to the agents for an autonomous run.
3. **Automated Gatekeeping:** Code is merged into the `main` branch if and only if:
    * It passes 100% of the required automated tests (unit, integration, and sandbox security constraints).
    * It successfully builds the required prototypes/demos.
    * Static analysis tools flag zero high-severity vulnerabilities.
4. **Agile Rollbacks:** If an agentic merge breaks the build or fails stakeholder evaluation during the demo phase, humans do not fix the code. We revert the commit, update the `nlspec` or test suite to catch the error, and trigger a new factory run.
