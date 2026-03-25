# Contributing to netfyr

Welcome to **netfyr**! We are building a Level 5 "dark software factory" for Linux host network management. 

Because our goal is **zero human code review**, contributing to this project looks fundamentally different from traditional open-source development. You will not be writing source code or reviewing pull requests. Instead, you will be writing requirements, crafting `nlspecs` (natural language specifications), and guiding autonomous AI agents to do the execution.

---

## Repository Structure: The Monorepo Approach

To give our AI agents the context they need and ensure perfect traceability between requirements and code, we keep everything in a single repository. However, we maintain strict boundaries between human spaces and AI spaces using folder structures.

* **/nlspec/ (The Human Input Zone):** This is where humans collaborate. It contains all architectural visions, feature requirements, and natural language specifications. *Agents read from here, humans write here.*
* **/src/ and /demos/ (The Dark Factory Output Zone):** This is where the code lives. Humans are not allowed to manually push code here. *Agents write here, humans only evaluate the output.*
* **.github/workflows/ (The Factory Engine):** Contains the CI/CD pipelines that trigger agent runs based on updates to the `nlspec` folder.

---

## Agile Workflow: Two Scrum Tracks

We run two parallel Scrum tracks: one for the **human team** and one for the **factory**. They operate on different cadences but feed into each other.

### Track 1: Human Scrum (1-week sprints)

The human sprint covers everything humans do to keep the factory running and productive. This includes:

- Writing and refining `nlspec` specifications
- Setting up and configuring target projects (repos, toolchains, factory configuration, `CLAUDE.md`)
- Improving the factory itself (pipeline logic, container presets, token budgets)
- Evaluating factory output (reviewing commits, triaging quarantined items, accepting or rejecting results)
- Managing the backlog of features and bugs across all projects

The human sprint is **not** about writing code. It is about defining what the factory should build, ensuring it has what it needs, and judging whether it succeeded.

#### Human Scrum Ceremonies

* **Daily Scrum (Mon-Fri, 1:30 PM CET | 15 minutes)**
    * **Focus:** Sync on factory run status, unblock specification ambiguities, review pipeline health, triage any quarantined items from overnight runs.
    * **Meeting Link:** [https://meet.google.com/xxx-xxxx-xxx](https://meet.google.com/xxx-xxxx-xxx)
* **Backlog Refinement (Tuesday, 1:45 PM CET | 45 minutes)**
    * **Focus:** Triage new GitHub issues, define success metrics for upcoming features, break down complex requirements into discrete `nlspec` drafts, and identify factory improvements needed (new presets, better prompts, tighter limits).
    * **Meeting Link:** [https://meet.google.com/yyy-yyyy-yyy](https://meet.google.com/yyy-yyyy-yyy)
* **Sprint Planning (Friday, 1:45 PM CET | 45 minutes)**
    * **Focus:** Decide which `nlspecs` are ready for the factory, which projects need setup work, and which factory improvements to prioritize. Queue up the weekend factory runs.
    * **Meeting Link:** [https://meet.google.com/zzz-zzzz-zzz](https://meet.google.com/zzz-zzzz-zzz)
* **Sprint Review & Retrospective (Monday, 1:45 PM CET | 60 minutes)**
    * **Focus:** Evaluate the factory's weekend output — review commits, run demos, inspect quarantined items. Then reflect on both factory performance and human process: *Were the specs precise enough? Did the factory hit token limits? Do we need to split specs differently? What project setup was missing?*
    * **Meeting Link:** [https://meet.google.com/aaa-aaaa-aaa](https://meet.google.com/aaa-aaaa-aaa)

### Track 2: Factory Sprints (overnight cycles)

The factory runs its own autonomous sprints. Each factory run is a sprint: it ingests specs, triages and orders them, then processes each one through understand → plan → implement → verify → commit. The factory produces a morning report summarizing what succeeded, what was quarantined, and what needs human attention.

Factory sprints are triggered by humans (typically at the end of Sprint Planning on Friday or on demand during the week) but run unattended. The daily cycle is:

```
     DAYTIME (humans)                         NIGHTTIME (factory)
     ================                         ===================

     1. Review morning report                 start
     2. Accept/reject commits                   |
     3. Resolve quarantined items             ingest (skip unchanged specs)
     4. Write new specs or revise old ones       |
     5. Set up projects as needed             triage & order
     6. Trigger the factory                      |
                                              [for each spec:]
                                              understand → plan → implement
                                                → verify → commit
                                                |
                                              report
                                                |
                                              exit
```

The two tracks connect at these points:
- **Human → Factory:** Sprint Planning produces the specs and project configs that feed the factory.
- **Factory → Human:** The morning report and git commits feed the Sprint Review. Quarantined items become backlog items for the next human sprint.

---

## Recommended Agentic Setup for Contributors

To effectively contribute to the `nlspec` folder or triage issues, we recommend using an **Agentic Terminal Setup**. We use a strict **fork-and-pull** workflow. You should not clone the upstream repository directly.

### The Recommended Stack: Claude Code + Local Fork
The most powerful way to craft requirements agentically is using a terminal-based AI like **Claude Code**.

**Step 1: Fork the Repository**
Navigate to `https://github.com/netfyr/netfyr` and click the "Fork" button in the top right corner to create a copy of the repository in your own GitHub account.

**Step 2: Clone Your Fork Locally**
```bash
git clone [https://github.com/YOUR-USERNAME/netfyr.git](https://github.com/YOUR-USERNAME/netfyr.git)
cd netfyr
git checkout -b feature/update-spec
```

**Step 3: Prompt the Agent to Edit Specs**
Instead of writing markdown manually, prompt your agent to research an issue and update the specifications. For example:
> *"Claude, please read issue #42 on the upstream netfyr repo. Based on that issue, update our `nlspec/network_engine.md` file to include the new requirement for Kubernetes CNI plugin support. Ensure the acceptance criteria are clearly defined."*

**Step 4: Review, Commit, and Push**
Let your local agent make the file edits in the `/nlspec/` directory. Briefly verify the changes, commit them with your sign-off, and push them to your fork.
```bash
git add nlspec/
git commit -m "docs: agentic update to network engine specs based on issue #42" -s
git push origin feature/update-spec
```

**Step 5: Open a Pull Request**
Go to your fork on GitHub and click "Compare & pull request" to submit your changes to the upstream `netfyr/netfyr` repository.

---

## The Contribution Workflow

If you want to add a feature or fix a bug in netfyr, follow this process:

1. **Open an Issue:** Describe the problem or feature requirement clearly in natural language on the upstream GitHub Issues board.
2. **Update the Specs (`/nlspec/`):** Use your local agentic setup on your forked repository (as described above) to update the relevant requirements documents so the factory knows *how* to solve the issue. Submit a PR from your fork to the upstream repo.
3. **Trigger the Factory:** Once your spec PR is merged by a Factory Operator (typically on Friday), the CI/CD pipeline will automatically trigger an autonomous agent run (e.g., using StrongDM's Attractor or RuFlo) for a 16-72 hour weekend cycle.
4. **Evaluate the Output:** During the Monday Sprint Review, evaluate the generated demos in the `/demos/` folder. If they fail, **do not edit the code**. Return to Step 1: open an issue detailing the failure and update the `nlspec`.

---

## Contributor Agreement (DCO)

Because humans do not write code here, we do not use a standard CLA. However, all prompt and spec contributions must be signed off to verify you have the right to submit them and that they do not contain malicious instructions.

Please read our `DCO.md` file. To sign off on your contributions, simply add the `-s` flag to your commits, or add the following to your commit messages:

> Signed-off-by: Your Name <your.email@example.com>
