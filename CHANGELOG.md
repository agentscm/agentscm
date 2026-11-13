# Changelog

All notable changes are listed here, newest first. AgentSCM is alpha: any release may change the CLI, the HTTP API, storage and schemas without a migration.

## [0.5.0](https://github.com/agentscm/agentscm/compare/v0.4.0...v0.5.0) - 2026-11-13

### Added

- *(hub)* Repair retained claims with findings, prune and work repair ([#574](https://github.com/agentscm/agentscm/pull/574))
- *(hook)* Complete merged Work when its session's turn ends ([#572](https://github.com/agentscm/agentscm/pull/572))
- *(hub)* Complete Work when the Hub observes its pull request merge ([#571](https://github.com/agentscm/agentscm/pull/571))
- *(hub)* Count a repository's deploy workflow run as its deployment ([#570](https://github.com/agentscm/agentscm/pull/570))
- *(hub)* Record script deploys with the Hub's own forge credential ([#566](https://github.com/agentscm/agentscm/pull/566))
- show source-bound change reviews with explicit sharing consent ([#568](https://github.com/agentscm/agentscm/pull/568))
- *(hub)* Let an embedder set conversation retention per repository ([#567](https://github.com/agentscm/agentscm/pull/567))
- *(hub)* Name each listed repository's forge host ([#565](https://github.com/agentscm/agentscm/pull/565))
- *(hub)* Let an embedder supply a connection's credential ([#564](https://github.com/agentscm/agentscm/pull/564))
- *(hub)* Ask the person whose merge an idle owner's PR holds ([#563](https://github.com/agentscm/agentscm/pull/563))
- *(hub)* Let an embedding program decide who may use the Hub ([#559](https://github.com/agentscm/agentscm/pull/559))
- *(hub)* Prompt the release of holds whose Work has merged ([#549](https://github.com/agentscm/agentscm/pull/549))
- *(hub)* Share agent conversations by default on a new Hub ([#554](https://github.com/agentscm/agentscm/pull/554))
- *(client)* Report Claude Code token usage by default ([#552](https://github.com/agentscm/agentscm/pull/552))
- *(cli)* [**breaking**] Remove the menu bar app and its activity feed ([#540](https://github.com/agentscm/agentscm/pull/540))
- *(client)* Capture Codex prompts, replies, tool results and usage ([#551](https://github.com/agentscm/agentscm/pull/551))
- *(hub)* Count cached input in usage totals and read stored reports ([#550](https://github.com/agentscm/agentscm/pull/550))
- *(hook)* Observe file changes made through shell tool calls ([#543](https://github.com/agentscm/agentscm/pull/543))
- *(hub)* Link Work to its pull request by branch and show its stage ([#536](https://github.com/agentscm/agentscm/pull/536))
- *(hub)* Record a start that names another person's Work ([#534](https://github.com/agentscm/agentscm/pull/534))
- *(hub)* Show Work intent and waiting dependencies ([#531](https://github.com/agentscm/agentscm/pull/531))
- *(hub)* Agree shared files and report Work dependencies ([#521](https://github.com/agentscm/agentscm/pull/521))
- *(hub)* Let an operator register a read-only connected app ([#525](https://github.com/agentscm/agentscm/pull/525))
- *(client)* Answer a Codex approval request from the Hub ([#526](https://github.com/agentscm/agentscm/pull/526))
- *(hub)* Let a connected app list and answer prompts ([#523](https://github.com/agentscm/agentscm/pull/523))
- *(client)* Send what every tool's agent asks to the Hub ([#519](https://github.com/agentscm/agentscm/pull/519))
- *(hub)* Give each Hub message a pick-up deadline ([#514](https://github.com/agentscm/agentscm/pull/514))
- *(client)* Wake an idle Codex chat with its person's Hub message ([#513](https://github.com/agentscm/agentscm/pull/513))
- *(hub)* Add an operator-registered context source for sessions ([#508](https://github.com/agentscm/agentscm/pull/508))
- *(pipeline)* Post pull request rule results on the head commit ([#503](https://github.com/agentscm/agentscm/pull/503))
- *(client)* Offer a subagent's prompt where its dialog shows ([#498](https://github.com/agentscm/agentscm/pull/498))
- *(pipeline)* Fire rules on pull request heads with a path rule ([#492](https://github.com/agentscm/agentscm/pull/492))
- *(hub)* Record which session started a session, and its subagents ([#490](https://github.com/agentscm/agentscm/pull/490))
- *(hub)* Mint an agent token for a local Hub without a forge ([#454](https://github.com/agentscm/agentscm/pull/454))
- *(hub)* List only core MCP tools by default ([#455](https://github.com/agentscm/agentscm/pull/455))
- *(hub)* Wake an idle agent when its person sends a Hub message ([#481](https://github.com/agentscm/agentscm/pull/481))
- *(hub)* Measure what contested-edit refusals lead to and hook cost ([#444](https://github.com/agentscm/agentscm/pull/444))
- *(hub)* Answer a prompt with its choices, a reason or answers ([#477](https://github.com/agentscm/agentscm/pull/477))

### Fixed

- *(hub)* Say why a Slack wait pass failed and keep the rest going ([#583](https://github.com/agentscm/agentscm/pull/583))
- *(client)* Keep capture working after macOS renumbers the disk ([#581](https://github.com/agentscm/agentscm/pull/581))
- *(hub)* Share conversations by default again and let people opt out ([#584](https://github.com/agentscm/agentscm/pull/584))
- *(cli)* Drop menu bar leftovers and bring the docs up to date ([#580](https://github.com/agentscm/agentscm/pull/580))
- *(hook)* Complete merged Work on a turn that asks something ([#579](https://github.com/agentscm/agentscm/pull/579))
- *(hub)* Count a turn's end as asking only on a real question ([#578](https://github.com/agentscm/agentscm/pull/578))
- *(hub)* Key repository rows by stable id, not name ([#576](https://github.com/agentscm/agentscm/pull/576))
- *(rules)* Say how every kind of agent session finishes its Work ([#577](https://github.com/agentscm/agentscm/pull/577))
- *(hub)* Read GitLab files as their exact bytes ([#555](https://github.com/agentscm/agentscm/pull/555))
- *(hub)* Keep GitLab MR labels, projects, base and fast-forward merges ([#553](https://github.com/agentscm/agentscm/pull/553))
- *(hub)* A waiting session names its newest unfinished task ([#558](https://github.com/agentscm/agentscm/pull/558))
- *(hub)* Deliver coordination replies and refuse unusable joins ([#548](https://github.com/agentscm/agentscm/pull/548))
- *(mcp)* Keep MCP through a Hub outage and announce lapsed holds ([#547](https://github.com/agentscm/agentscm/pull/547))
- *(hub)* Say a lead cannot answer only when its session is gone ([#545](https://github.com/agentscm/agentscm/pull/545))
- *(hub)* Accept the CLI's short base ref in local-validation summaries ([#546](https://github.com/agentscm/agentscm/pull/546))
- Name the offending field in Work request errors and scope refusals ([#544](https://github.com/agentscm/agentscm/pull/544))
- *(hub)* Show inline work views only on request ([#537](https://github.com/agentscm/agentscm/pull/537))
- *(hub)* Accept session on every agent tool ([#533](https://github.com/agentscm/agentscm/pull/533))
- *(client)* Capture a shell call another hook rewrote or a large tree ([#528](https://github.com/agentscm/agentscm/pull/528))
- *(hub)* Read a turn that ends on a report as asking nothing ([#527](https://github.com/agentscm/agentscm/pull/527))
- *(client)* Free accepted conversation bodies from a capture store ([#524](https://github.com/agentscm/agentscm/pull/524))
- *(hub)* Keep a session's question listed after the session ends ([#522](https://github.com/agentscm/agentscm/pull/522))
- *(hub)* Keep message facts across a restart; refuse a second waiter ([#517](https://github.com/agentscm/agentscm/pull/517))
- *(client)* Accept Claude Code's orphaned marker in the plugin cache ([#520](https://github.com/agentscm/agentscm/pull/520))
- *(hub)* Keep Work active across a session end while its hold is live ([#515](https://github.com/agentscm/agentscm/pull/515))
- *(client)* Expire capture facts that never become gaps and done claims ([#518](https://github.com/agentscm/agentscm/pull/518))
- *(hub)* Put the operator context in the structured start reply ([#516](https://github.com/agentscm/agentscm/pull/516))
- *(hub)* End only deliveries with unread records below the floor ([#506](https://github.com/agentscm/agentscm/pull/506))
- *(hub)* Offer Hub messages to a session known only through MCP ([#510](https://github.com/agentscm/agentscm/pull/510))
- *(client)* Tidy the capture store before writes; say when it is full ([#511](https://github.com/agentscm/agentscm/pull/511))
- *(hub)* Name the Work to pass when a request wakes its lead ([#512](https://github.com/agentscm/agentscm/pull/512))
- *(hub)* Keep waits and offered prompts across a Hub restart ([#507](https://github.com/agentscm/agentscm/pull/507))
- *(hub)* Wake a lead for a request to Work in another repository ([#509](https://github.com/agentscm/agentscm/pull/509))
- *(hub)* Let an agent decline a Hub message it accepted ([#496](https://github.com/agentscm/agentscm/pull/496))
- *(hub)* Do not ask a person about a turn that ended asking nothing ([#505](https://github.com/agentscm/agentscm/pull/505))
- *(hub)* Keep a waiting prompt across a renewed agent token ([#504](https://github.com/agentscm/agentscm/pull/504))
- *(hub)* Name a session in the activity read as Work replies do ([#501](https://github.com/agentscm/agentscm/pull/501))
- *(hub)* Tell a person on Slack when their own session waits for them ([#497](https://github.com/agentscm/agentscm/pull/497))
- *(hub)* Wake and escalate coordination requests a busy lead ignores ([#495](https://github.com/agentscm/agentscm/pull/495))
- *(hub)* Count overhead totals a write has taken as missing ([#494](https://github.com/agentscm/agentscm/pull/494))
- *(ci)* Stop merge-admission sweeps resetting and duplicating the gate ([#491](https://github.com/agentscm/agentscm/pull/491))
- *(hook)* Say coordination unknown for an edit it cannot place ([#453](https://github.com/agentscm/agentscm/pull/453))
- *(hub)* List the newest unfinished Work first and let callers page ([#489](https://github.com/agentscm/agentscm/pull/489))
- *(hub)* List a confirmed prompt on the answer page at once ([#488](https://github.com/agentscm/agentscm/pull/488))
- *(client)* Say why sharing is off and keep the capture store working ([#479](https://github.com/agentscm/agentscm/pull/479))
- *(hub)* Stop counting what a person dismissed or snoozed ([#487](https://github.com/agentscm/agentscm/pull/487))
- *(client)* Keep a worktree session to one agent ([#485](https://github.com/agentscm/agentscm/pull/485))
- *(client)* Renew connected checkouts when signing in again ([#482](https://github.com/agentscm/agentscm/pull/482))
- *(client)* Find a linked worktree's connection in check commands ([#474](https://github.com/agentscm/agentscm/pull/474))
- *(hub)* Keep a permission offer answerable after a gap ([#475](https://github.com/agentscm/agentscm/pull/475))

### Other

- *(client)* Use Windows paths in session-route and hook-report tests ([#588](https://github.com/agentscm/agentscm/pull/588))
- *(hub)* Read the GitLab publication by host in shared-name tests ([#585](https://github.com/agentscm/agentscm/pull/585))
- *(hub)* Run the hook overhead tests one at a time on an empty table ([#575](https://github.com/agentscm/agentscm/pull/575))
- *(hook)* Wait for the relay before the real-Hub session hooks ([#569](https://github.com/agentscm/agentscm/pull/569))
- *(hook)* Cover subagent scope refusals and resumed recovery ([#562](https://github.com/agentscm/agentscm/pull/562))
- Let an idle owner's open PR stop blocking a merge ([#560](https://github.com/agentscm/agentscm/pull/560))
- *(hook)* Expect the root in the working directory's spelling ([#557](https://github.com/agentscm/agentscm/pull/557))
- Judge merge readiness by each check's newest run ([#502](https://github.com/agentscm/agentscm/pull/502))
- *(hub)* Share the settings SQL between both stores ([#499](https://github.com/agentscm/agentscm/pull/499))
- *(hook)* Wait for the relay to pool before timing hooks ([#493](https://github.com/agentscm/agentscm/pull/493))
- Keep git maintenance out of the collision history tests ([#478](https://github.com/agentscm/agentscm/pull/478))
- Admit a pull request to the queue only after its own checks pass ([#469](https://github.com/agentscm/agentscm/pull/469))

## [0.4.0](https://github.com/agentscm/agentscm/compare/v0.3.0...v0.4.0) - 2026-11-10

### Added

- *(hub)* Let a person allow any request of their own agent ([#464](https://github.com/agentscm/agentscm/pull/464))
- *(hub)* Show agents Work in their person's other repositories ([#443](https://github.com/agentscm/agentscm/pull/443))
- *(hub)* Answer a waiting permission prompt from the Hub ([#448](https://github.com/agentscm/agentscm/pull/448))
- *(hub)* Read forge coordination and Slack setup for a repository ([#400](https://github.com/agentscm/agentscm/pull/400))
- *(hub)* Read whether a repository is watched after merge ([#401](https://github.com/agentscm/agentscm/pull/401))
- *(hub)* Read a repository's retention limits and stand-up usage ([#399](https://github.com/agentscm/agentscm/pull/399))
- *(hub)* Let a person connect an app that reads and answers for them ([#405](https://github.com/agentscm/agentscm/pull/405))
- *(hub)* Say what an overlap alert is about in its text ([#427](https://github.com/agentscm/agentscm/pull/427))
- *(hub)* Let a person message their own running agent session ([#413](https://github.com/agentscm/agentscm/pull/413))
- *(hub)* Add a weekly private digest of stale Work and collisions ([#429](https://github.com/agentscm/agentscm/pull/429))
- *(client)* State per-harness coverage and name unwired agents ([#430](https://github.com/agentscm/agentscm/pull/430))
- *(hub)* Observe pr-gate CI, approval and merge; post the skill status ([#440](https://github.com/agentscm/agentscm/pull/440))
- *(hub)* Answer why a commit or file was built ([#442](https://github.com/agentscm/agentscm/pull/442))
- *(client)* Route MCP calls by path and fill in the session ([#426](https://github.com/agentscm/agentscm/pull/426))
- *(cli)* Add collisions --history to count past overlaps ([#425](https://github.com/agentscm/agentscm/pull/425))
- *(validation)* Select tests by path prefix within a command's paths ([#419](https://github.com/agentscm/agentscm/pull/419))
- *(hub)* Retain validation evidence and maintain one PR summary ([#417](https://github.com/agentscm/agentscm/pull/417))
- *(hub)* Re-land the actionable error envelope and connection states ([#416](https://github.com/agentscm/agentscm/pull/416))
- *(hub)* Make conversation sharing a repository setting ([#410](https://github.com/agentscm/agentscm/pull/410))
- *(hub)* Let people dismiss and snooze attention items ([#409](https://github.com/agentscm/agentscm/pull/409))
- *(hub)* Let agents describe Work beside a short title ([#407](https://github.com/agentscm/agentscm/pull/407))
- *(hub)* Report sessions that wait for their person ([#404](https://github.com/agentscm/agentscm/pull/404))
- *(hub)* Let a Hub page ask the web sign-in to return to it ([#393](https://github.com/agentscm/agentscm/pull/393))

### Fixed

- *(client)* Build on Windows again ([#465](https://github.com/agentscm/agentscm/pull/465))
- *(hub)* Ask a person only for what agents cannot settle ([#452](https://github.com/agentscm/agentscm/pull/452))
- *(client)* Do not check an edit that resolves a Git conflict ([#449](https://github.com/agentscm/agentscm/pull/449))
- *(hub)* Sanitize relayed titles; message only top-level sessions ([#450](https://github.com/agentscm/agentscm/pull/450))
- *(hub)* Stop calling a task quiet while its agent works or waits ([#451](https://github.com/agentscm/agentscm/pull/451))
- *(ci)* Report the merge gate as a commit status too ([#447](https://github.com/agentscm/agentscm/pull/447))
- *(client)* Fast hooks, worktree routing, prompt and reply recovery ([#432](https://github.com/agentscm/agentscm/pull/432))
- *(hub)* Ask a session to declare its Work at its first edit ([#431](https://github.com/agentscm/agentscm/pull/431))
- *(hub)* Advise patching other lines before asking for a held file ([#422](https://github.com/agentscm/agentscm/pull/422))
- *(hub)* Name the refusing limit and let busy repositories compact ([#441](https://github.com/agentscm/agentscm/pull/441))
- *(hub)* State the shared-body read rule and fix two misleading reads ([#433](https://github.com/agentscm/agentscm/pull/433))
- *(deps)* Raise rusqlite past RUSTSEC-2026-0459 ([#436](https://github.com/agentscm/agentscm/pull/436))
- *(cli)* Read Hub replies that a newer Hub has added to ([#428](https://github.com/agentscm/agentscm/pull/428))
- *(hub)* Finish Work from agentscm_done and show what agents wait for ([#418](https://github.com/agentscm/agentscm/pull/418))
- *(validation)* Check Node before queueing an attestation ([#420](https://github.com/agentscm/agentscm/pull/420))
- *(hub)* Do not flag a pull request against the agent that opened it ([#412](https://github.com/agentscm/agentscm/pull/412))
- *(hub)* List Work ended by a transition among recent completions ([#414](https://github.com/agentscm/agentscm/pull/414))
- *(hub)* Read an oversize pull request file list as unknown paths ([#411](https://github.com/agentscm/agentscm/pull/411))
- *(hub)* Announce an intent once and keep a session's Work title ([#406](https://github.com/agentscm/agentscm/pull/406))

### Other

- Build once, test in shards, lint in parallel, warm the gate cache ([#461](https://github.com/agentscm/agentscm/pull/461))
- Keep the tested arm64 image and a macOS client per main commit ([#458](https://github.com/agentscm/agentscm/pull/458))
- Assert meaning, not other changes' wording or counts ([#463](https://github.com/agentscm/agentscm/pull/463))
- Say what gates a merge and drop per-PR attestation steps ([#462](https://github.com/agentscm/agentscm/pull/462))
- Update the roadmap from merged pull requests ([#457](https://github.com/agentscm/agentscm/pull/457))
- *(deps)* bump the actions group across 1 directory with 11 updates ([#438](https://github.com/agentscm/agentscm/pull/438))
- *(hub)* Pin the preflight as readable with a read-only token ([#402](https://github.com/agentscm/agentscm/pull/402))
- *(relay)* Tolerate a request sent before the pool close is seen ([#446](https://github.com/agentscm/agentscm/pull/446))
- Fix two merge-batch flakes in setup status and fake Docker ([#434](https://github.com/agentscm/agentscm/pull/434))
- Give the store consolidation design its page title ([#439](https://github.com/agentscm/agentscm/pull/439))
- Design ending the double Hub store implementation ([#437](https://github.com/agentscm/agentscm/pull/437))
- Run the checks on every pull request ([#435](https://github.com/agentscm/agentscm/pull/435))
- Run PostgreSQL tests in parallel, one database per test ([#421](https://github.com/agentscm/agentscm/pull/421))
- Add a setup prompt to paste into a coding agent ([#424](https://github.com/agentscm/agentscm/pull/424))
- Lint in merge batches and make the signed check optional ([#423](https://github.com/agentscm/agentscm/pull/423))

## [0.3.0](https://github.com/agentscm/agentscm/compare/v0.2.1...v0.3.0) - 2026-11-06

### Added

- *(hub)* Record hand-off pickups when the person's agent starts ([#390](https://github.com/agentscm/agentscm/pull/390))
- *(hub)* Confirm the GitHub App owner and name on the setup page ([#299](https://github.com/agentscm/agentscm/pull/299))
- *(hub)* Badge broken agent setup and expired agent tokens ([#362](https://github.com/agentscm/agentscm/pull/362))
- *(hub)* Inspect, release and recover Work from agents and the Hub ([#366](https://github.com/agentscm/agentscm/pull/366))
- *(hub)* End a session and pause its Work in one transaction ([#359](https://github.com/agentscm/agentscm/pull/359))
- *(validation)* Prepare selected PR tests and merge-batch gate ([#374](https://github.com/agentscm/agentscm/pull/374))
- *(local-validation)* Publish independent signed checks per PR ([#372](https://github.com/agentscm/agentscm/pull/372))
- *(hub)* Message the person in Slack when work becomes blocked or offered ([#358](https://github.com/agentscm/agentscm/pull/358))
- *(hub)* Report capacity refusals to the operator and match tray needs ([#351](https://github.com/agentscm/agentscm/pull/351))
- *(hub)* Make default start and overview MCP replies compact ([#363](https://github.com/agentscm/agentscm/pull/363))
- *(cli)* Suggest the merge order that needs the fewest new heads ([#356](https://github.com/agentscm/agentscm/pull/356))
- *(hub)* show planned issues and link native Work ([#361](https://github.com/agentscm/agentscm/pull/361))
- *(hub)* Show failed required checks and stopped gates in Needs you ([#348](https://github.com/agentscm/agentscm/pull/348))
- *(hub)* Deliver required agent rules and link sessions to Work ([#344](https://github.com/agentscm/agentscm/pull/344))
- *(hub)* Let a person close a request whose requester is gone ([#349](https://github.com/agentscm/agentscm/pull/349))
- *(hub)* Name the holding session and keep hostnames out of labels ([#342](https://github.com/agentscm/agentscm/pull/342))
- *(hub)* Lapse idle Work reservations on a server-timed lease ([#330](https://github.com/agentscm/agentscm/pull/330))
- *(hub)* Share actionable status across work surfaces ([#340](https://github.com/agentscm/agentscm/pull/340))
- *(hub)* Keep the Work lead across a token change ([#337](https://github.com/agentscm/agentscm/pull/337))
- *(hub)* Reconcile PR evidence without hiding held Work ([#336](https://github.com/agentscm/agentscm/pull/336))
- *(hub)* Report PR evidence and prerequisites for a Work item ([#332](https://github.com/agentscm/agentscm/pull/332))
- *(hub)* Refuse ambiguous omitted sessions and free paused sessions ([#327](https://github.com/agentscm/agentscm/pull/327))
- *(hub)* Separate durable Work outcomes from scope reservations ([#317](https://github.com/agentscm/agentscm/pull/317))
- Enroll and revoke trusted local CI devices ([#307](https://github.com/agentscm/agentscm/pull/307))

### Fixed

- Build on Windows and expect the recovery tools for read-only tokens ([#385](https://github.com/agentscm/agentscm/pull/385))
- *(skill-workflow)* Do not cancel a live action on lease contention ([#298](https://github.com/agentscm/agentscm/pull/298))
- *(ui)* Drop Retry when the Hub has workspace sign-in off ([#297](https://github.com/agentscm/agentscm/pull/297))
- *(validation)* Serialize heavy jobs with a visible FIFO ([#380](https://github.com/agentscm/agentscm/pull/380))
- *(ci)* Call the Octokit method that lists a ref's commit statuses ([#377](https://github.com/agentscm/agentscm/pull/377))
- *(hub)* Show other owners' sessions by digest in structured replies ([#350](https://github.com/agentscm/agentscm/pull/350))
- *(cli)* Make doctor, usage errors and token messages readable ([#296](https://github.com/agentscm/agentscm/pull/296))
- *(hub)* Let agents settle coordination instead of asking the user ([#326](https://github.com/agentscm/agentscm/pull/326))
- *(hub)* Show coordination requests in the session the person types in ([#320](https://github.com/agentscm/agentscm/pull/320))
- *(hub)* Stop MCP app views resizing in a loop ([#319](https://github.com/agentscm/agentscm/pull/319))
- *(hub)* Log why requests are rejected as invalid ([#318](https://github.com/agentscm/agentscm/pull/318))

### Other

- *(hook-relay)* Accept a reset as a closed standby link on Linux ([#396](https://github.com/agentscm/agentscm/pull/396))
- Back out #387 and #388 until they pass the full suite ([#395](https://github.com/agentscm/agentscm/pull/395))
- *(roadmap)* Apply Roadmap lines merged since v0.2.1 ([#383](https://github.com/agentscm/agentscm/pull/383))
- *(hook)* Reuse a pooled Hub connection for command hooks ([#384](https://github.com/agentscm/agentscm/pull/384))
- *(local-validation)* Document a warm build cache for attestations ([#376](https://github.com/agentscm/agentscm/pull/376))
- *(test)* Optimize dependencies and shorten deliberate test waits ([#373](https://github.com/agentscm/agentscm/pull/373))
- *(process)* Update the roadmap once per release, not in every PR ([#354](https://github.com/agentscm/agentscm/pull/354))
- *(validation)* Skip the Rust gate for Markdown-only changes ([#353](https://github.com/agentscm/agentscm/pull/353))
- *(ci)* Move Hub qualification to local release checks ([#371](https://github.com/agentscm/agentscm/pull/371))
- *(quickstart)* Find the doctor's Unverified line among its findings ([#357](https://github.com/agentscm/agentscm/pull/357))
- Remove ci-extended as a routine pull request step ([#341](https://github.com/agentscm/agentscm/pull/341))
- Reduce the local cost of validation and agent PR workflows ([#331](https://github.com/agentscm/agentscm/pull/331))
- Lock attestations per machine and add interim agent merge rules ([#321](https://github.com/agentscm/agentscm/pull/321))
- Add PR attest script and pin the verifier to a release ([#308](https://github.com/agentscm/agentscm/pull/308))

## [0.2.1](https://github.com/agentscm/agentscm/compare/v0.2.0...v0.2.1) - 2026-11-03

### Fixed

- *(release)* Upload the whole UI build for release binaries ([#304](https://github.com/agentscm/agentscm/pull/304))

## [0.2.0](https://github.com/agentscm/agentscm/compare/v0.1.3...v0.2.0) - 2026-11-03

### Added

- *(ui)* count requests waiting on you in the Inbox badge ([#288](https://github.com/agentscm/agentscm/pull/288))
- Add recorded contributions across Hub and agent surfaces ([#250](https://github.com/agentscm/agentscm/pull/250))
- *(hub)* tell the owner when a request waits on their idle agent ([#286](https://github.com/agentscm/agentscm/pull/286))
- *(hub)* refresh the in-agent MCP views on their own ([#264](https://github.com/agentscm/agentscm/pull/264))
- *(ui)* clearer heading roles and one Needs you row for overlaps ([#255](https://github.com/agentscm/agentscm/pull/255))
- Define immutable local CI plans and required coverage ([#277](https://github.com/agentscm/agentscm/pull/277))
- show repository connections across revisions ([#266](https://github.com/agentscm/agentscm/pull/266))
- Add managed agent continuation and operating policy
- add continuity preparation for GitHub and GitLab ([#265](https://github.com/agentscm/agentscm/pull/265))
- *(ui)* add an opt-in task-first layout over live Hub data ([#261](https://github.com/agentscm/agentscm/pull/261))
- *(hub)* thread the Inbox and let people answer for their own agent ([#252](https://github.com/agentscm/agentscm/pull/252))
- Add source maps and authored change walkthroughs
- *(hub)* restyle the in-agent MCP views to the Twilight panel ([#251](https://github.com/agentscm/agentscm/pull/251))
- *(ui)* rework All repositories and quiet the first-visit tour ([#249](https://github.com/agentscm/agentscm/pull/249))
- *(ui)* lead the Dashboard with Needs you, compact rows and tips ([#244](https://github.com/agentscm/agentscm/pull/244))
- show agent work across repositories in tray panel
- Add taskbar companion with Hub attention and CLI prompts ([#238](https://github.com/agentscm/agentscm/pull/238))
- connect PR evidence with Work and branch advice ([#245](https://github.com/agentscm/agentscm/pull/245))
- *(hub)* greet web sign-ins by profile first name ([#243](https://github.com/agentscm/agentscm/pull/243))
- *(ui)* Make the installed Hub app feel native ([#242](https://github.com/agentscm/agentscm/pull/242))
- *(ui)* rework the Hub sign-in, work pages and agent hand-off ([#240](https://github.com/agentscm/agentscm/pull/240))
- Add an All repositories page to the Hub web UI ([#236](https://github.com/agentscm/agentscm/pull/236))

### Fixed

- *(hub)* Cut old content from agent windows and the Hub ([#287](https://github.com/agentscm/agentscm/pull/287))
- *(ui)* make agent names, pills and disclosures consistent ([#260](https://github.com/agentscm/agentscm/pull/260))
- *(ui)* polish after the Nov 1-2 Hub UI merges ([#276](https://github.com/agentscm/agentscm/pull/276))
- Require signed local validation for routine CI ([#279](https://github.com/agentscm/agentscm/pull/279))
- *(hub)* make Slack direct messages match the Hub's wording and links ([#263](https://github.com/agentscm/agentscm/pull/263))
- *(ui)* reuse the Hub's palette, pill and helpers in the G layout ([#262](https://github.com/agentscm/agentscm/pull/262))

### Other

- enroll jhawlwut's local-validation signing key ([#283](https://github.com/agentscm/agentscm/pull/283))
- *(hub)* cache role checks for the All repositories list ([#241](https://github.com/agentscm/agentscm/pull/241))
- Cut per-call work in capture, status and hub paths ([#231](https://github.com/agentscm/agentscm/pull/231))

## [0.1.3](https://github.com/agentscm/agentscm/compare/v0.1.2...v0.1.3) - 2026-10-29

### Fixed

- Use GNU release tooling for changelog preparation ([#232](https://github.com/agentscm/agentscm/pull/232))
- Repair Work namespaces after repository renames ([#230](https://github.com/agentscm/agentscm/pull/230))

## 0.1.2 (2026-10-29)

- Make local validation comments clearly attributable to AgentSCM and report missing PostgreSQL/S3 coverage only when the repository policy requires it.
- Reduce hook, MCP and Hub memory use by removing repeated scans and bounding retained buffers.
- Bound contributor build storage and consolidate integration-test linking.
- Prepare releases from Cargo versions and Git tags with version-free PR text; keep binary/container qualification and npm approval in the existing publishing workflow.
- Known limits: the software remains alpha. Platform, service and live runtime coverage depend on the release qualification jobs; a passing local suite alone does not establish them.

## 0.1.0-alpha.9 (2026-10-28)

- The repository moved to `agentscm/agentscm`. Installers, npm metadata, container and plugin references and the local-validation bindings now name it; release jobs check the repository and organization by their IDs, and historical releases and attestations are unchanged (#204, #206).
- An agent's first session in a newly connected checkout has every AgentSCM MCP tool: the server lists the full tool list before the checkout is connected, and a call made before the connection exists says why in plain words (connecting, not signed in, repository not on this Hub, computer revoked) (#201).
- `agentscm --help` lists only the everyday commands; experimental commands still run but are hidden. Quickstart shows `--hub-url`, `--yes`, `--only`, `--dry-run` and `--json`; the other flags still work, hidden. The binary itself accepts `agentscm --hub-url URL` like `npx agentscm` (#186), and `--version` shows the build commit (#198) (#202).
- After a first sign-in, `client connect`, `client setup` and quickstart mint agent tokens through the computer, so revoking the computer stops that checkout's agents at once (#200).
- Hub UI: layout, link and wording fixes from a visual pass of every page (#185).
- Contributors: `tools/windows-check.sh` compiles the Windows build from macOS or Linux (#208).
- `npx agentscm` asks nothing once the Hub is known (`--hub-url`, the checkout's connection, or the computer's only sign-in); the "Where should AgentSCM run?" choice remains only without one. It sets up every coding agent it finds, by program on PATH or by configuration directory (Claude Code and Codex by program, since their plugins install through their CLIs; Cursor, GitHub Copilot and Gemini CLI by either), for all projects, also from a directory that is not a repository; `--agent` narrows the list. The "all your projects?" question and its No path, which dropped Claude Code and Codex, are gone (#188), as are "Go ahead?", the auto-connect question (auto-connect is always on), the collision check (`--no-collisions` is accepted and does nothing), the menu bar question (a closing line names `agentscm tray`) and the native Hub install confirmation. The closing lines say what was set up, to fully quit and reopen the agents, to trust Codex's hooks with `/hooks` when Codex was set up, and the undo command, which no longer names a program in a temporary directory.
- Claude Code runs AgentSCM's MCP tools without asking: setup adds `mcp__plugin_agentscm_agentscm__*` to `permissions.allow` in `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR`), keeping the old file as `settings.json.bak`, and `agentscm uninstall` takes out exactly that rule when setup added it. Gemini CLI's AgentSCM server gets `"trust": true`. Other agents have no documented per-server setting, so they still ask.
- `--yes` and runs without a terminal sign in: the link is printed on stderr, the browser opens when it can, and quickstart waits; a sign-in that cannot finish there says to run quickstart once in your own terminal (#187). A new local Hub waits the same way for the GitHub App to be created when a browser can open, and exits 3 as before when none can. A GitHub login that `gh` reports is used without asking.
- Joining a Hub, quickstart says which Hub it joins instead of local-Hub platform and Docker details (#189).
- `client setup` points the user-wide plugin and hooks at `agentscm` on PATH or at the stable copy in `~/.agentscm/bin`, never at a build directory or npm cache (#190).
- The user-wide hooks no longer add a "coordination unknown" note when an edited file is in no connected checkout (#199). A repository with no connection still gets the one-line hint at session start.

## 0.1.0-alpha.8 (2026-10-27)

- Fixed: the Hub in a quickstart-managed Docker container never became healthy, because quickstart sets `AGENTSCM_SLACK_FILE` to an empty value and the Hub tried to load Slack settings from that empty path. An empty value now means no Slack file (#181).
- Fixed: the Linux (musl) binaries did not compile, because the date formatting named `libc::time_t`, which is deprecated on musl. 0.1.0-alpha.7 was never released for that reason (only its container image was published); this release is everything listed under alpha.7 plus this fix.

## 0.1.0-alpha.7 (2026-10-27, not released)

- The Hub's health check (`GET /v1/hub/health`, `/health`) reports its `version` and build `commit`, and `agentscm hub whoami` prints them next to its own, saying when the CLI or the Hub is older. Builds take the commit from Git, or from the `AGENTSCM_BUILD_COMMIT` environment variable where there is no `.git` (the Docker build).
- Fixed: a second build from main could not be set up. Every build carries the release's version, so quickstart refused to overwrite `~/.agentscm/bin/agentscm-<version>` and skipped agent setup. Builds other than the tagged release now install as `agentscm-<version>+<commit>`, and a failed copy makes quickstart exit non-zero instead of continuing.
- Setup never writes to the repository. `agentscm quickstart` no longer offers to write the agent instruction files; pass `--init` or run `agentscm init-repo` for them. Those files no longer name a Hub or per-agent `client setup` commands: they tell each developer to run `agentscm quickstart`, since the Hub is set per machine. `client setup` and `client check` print the signed-in account as `login (GitHub)` and the token expiry as a date instead of an epoch number.
- Experimental, fake-tested only (live verification pending separate authority): a non-stacked GitHub merge group gets the `agentscm/workflow/<target>` `success` only when the group commit's first parent is the event's base (the candidate baseline) and any second parent is the queued head. A single parent that is not the base (as with rebase-method multi-commit pull requests) stays pending as unsupported; zero parents, more than two, or two that do not match fail. AgentSCM does not post the status on GitLab merged-result pipeline or merge train commits, so the GitLab status stays on the exact MR head and says nothing about the merged result or train. See [GitHub merge queues](docs/advanced/github-merge-queue.md).
- `POST /v1/hub/session/read-token` gives a web sign-in a 30-minute read-only token for one repository, under the same allow-list and role checks as a computer's mint, so the Hub's web pages can read the dashboard, inbox and history without a pasted token. Only the cookie sign-in with its CSRF token may ask. The bundled UI does not call it yet.
- The Hub API can sign a browser in itself (`/v1/hub/login/{provider}?web=1`) with an `HttpOnly`, `Secure`, `SameSite=Strict` cookie whose digest is stored in the database, so a Hub restart or deploy does not end it. It lasts 8 hours from the last use and at most 7 days. The bundled web UI does not use it yet; it still asks for a pasted token. Every change made with it needs the `csrf_token` that `GET /v1/hub/session` returns, sent as `x-csrf-token`. It counts as a human session (the computer list and revoke, and admin routes for administrators), never mints agent tokens or computer credentials, and ends with `DELETE /v1/hub/session` or when the person is disabled or leaves the allow list. New named Hub migration `browser_sessions`.

- Experimental: sign in once per computer. `agentscm hub login` and the menu bar app's new **Sign In with GitHub…** also sign the computer in, with a revocable credential the Hub stores as a digest, so a Hub restart signs nobody out and there is no 8-hour or 30-day limit (unused for 90 days, it stops). Sign-in opens the browser on desktops (a code over SSH, or with `--device`), says while it waits, gives up after 3 minutes with what to do, and every failed step names what failed. Agent tokens minted through the computer last 24 hours and the hook renews them in the background, re-checking the allow list, the account and the repository role with the connection's own GitHub or GitLab credential under the same rule as sign-in; a running MCP server keeps working across a renewal. Refusals (`insufficient_role` and the others) carry the Hub's sentence to the agent's context, the menu bar app (**Needs attention**), `agentscm open` and the CLI; an outage is not a refusal. With `agentscm hub auto-connect on` (asked once at sign-in; `--no-auto-connect` says no), an agent starting in an unconnected checkout of a connected repository connects it and gets `AgentSCM: coordinating on owner/name.`, writing no file in the checkout; a fork of a connected repository gets a hint naming it. New `agentscm hub whoami` and `agentscm hub devices list|rename|revoke`; `client doctor` verifies the connection through the computer; `hub logout` signs the computer out; the local Hub page lists and revokes computers; admin routes list computers, connected-repository usage and deduplicated refused sign-ins. The GitHub App setup page links to its **Enable Device Flow** checkbox, which a manifest cannot set. Tray Connect signs in when needed (#116). New named Hub migration `devices`; a refused browser sign-in now redirects to the Hub's signed-in page with the reason. See [connect this computer](docs/connect-this-computer.md#sign-in-once-per-computer).
- Local validation: the producer now computes pass or fail against the policy's `required_services` instead of always requiring PostgreSQL and S3, so a policy that requires neither can produce a passing envelope. A missing service is still listed as a coverage omission, and a missing service the policy requires still fails.
- Local validation: a recipe may declare optional `dependency_files` and `toolchain_probes` so non-Rust repositories (for example a Bun repository with `bun.lock`) can bind their own lockfiles and tool versions into the signed evidence. Recipes without them keep their digest and the Rust defaults.
- Local validation: a recipe command may declare `paths` (Git pathspecs) so `run` runs and records only the commands the change from the merge-base requires (the rest are `not_required`, reported as `not required: <id>`), and the policy may declare `exempt_paths`: with it, `reconcile` checks every open pull request on the base and sets `success` (`not required: only exempt paths changed`) on a head whose non-empty change touches only exempt paths, with no attestation. `verify` and `reconcile` recompute the required set from Git and fail on a mismatch. A change to `--config-dir` or `--verifier-workflow` (new on `run`, `verify` and `reconcile`) requires every command and is never exempt. The workflow also runs on `pull_request_target` (opened, synchronize, reopened, and edits that change the base), so an exempt or retargeted pull request is refreshed promptly. Recipes and policies without the fields keep their digests, but an older AgentSCM binary rejects ones that use them, so upgrade a pinned version first; adding either field changes the digest and outstanding attestations stop matching. Tested only against a fake GitHub.
- Local validation: new `agentscm local-validation reconcile` sets the `agentscm/local-validation` status of every open pull request on a base branch from its signed attestation comments: `pending` first on every opted-in head, heads fetched as Git data through an isolated per-run transport (anonymous by default, or with explicit `--github-authenticated-fetch` for one canonical private GitHub HTTPS origin), pull requests and comments re-read before each write, shared heads left `pending` as unsupported, incomplete listings failing closed, a head that cannot be fetched left `pending` with exit 1, and no verdict written when the default branch's verifier configuration directory changed since checkout (`--config-dir` is relative to `--repo`). `.github/workflows/local-validation.yml` is now a thin wrapper around it with one repository-wide job-level concurrency group and no `workflow_dispatch`; every run sweeps all open pull requests. The authenticated mode reuses the selected API token only in the fetch child's runtime Git environment, with credential helpers, prompts, redirects, hooks, executable transports and ambient Git configuration disabled; the token is absent from argv and persistent Git configuration. Reconciliation is tested against a fake GitHub, real local bare Git transports and a test-only authenticated command boundary, not live private GitHub, redirects or transport timeouts; the first live Actions run remains after merge.
- Local validation: a reusable `workflow_call` job installs one exact published AgentSCM npm version with provenance required and lifecycle scripts disabled, checks the installed binary directly, and runs the full reconciliation sweep. Callers must pin both an exact SemVer and the workflow's full commit SHA; the existing source-build verifier and documentation placeholders remain until that release is published and qualified. The job accepts only an explicit boolean for authenticated fetch and reuses its job-scoped GitHub token; it accepts no PAT, GitHub App secret, OIDC input or inherited secrets. Actionlint, offline zizmor and synthetic local runners passed, but live GitHub Actions scheduling, public adoption and private fetch are not qualified.
- Release CI now checks `local-validation reconcile --help` and the `--github-authenticated-fetch` flag on each native Unix release binary. This is only a command-surface smoke test; it does not perform reconciliation, authenticate to GitHub or prove installer provenance. Windows does not support reconciliation, and the macOS x86_64 binary is cross-compiled rather than executed.

- Conversation sharing is one step for a connected checkout. The macOS tray's **Share agent conversations with Hub** switch no longer needs `change-capture configure` first: turning it on creates the capture binding with documented defaults through the `configure` code path (private per-checkout spool, `origin/main`, one `Bash=execution:partial:process_exit_code` rule, caller-asserted setup digest), re-enables a disabled binding, or just changes the mode of an enabled one, and its status window says what sharing does. The Hub page opened from this computer (`agentscm open`) has the same switch under **Settings → Conversation sharing**, backed by `GET`/`POST /v1/local/capture`: the change needs the page's own origin and live connection, names the checkout by its listed repository and Hub, and turning sharing on needs an explicit confirmation. The tray, the page and the client status read the setting through one function. Fixed: the repository explorer failed to load a session captured in `shared-conversation` mode.

- macOS tray: a Git checkout where a coding agent's client-connection hook ran without a client connection is listed under **Not connected** with its repository, agent and last-seen time, and **Connect with** that agent runs `client setup` for its main checkout with the most recent saved Hub sign-in, then shows the result. The hook reports only checkouts whose `origin` is a supported forge repository, and only while a tray is listening; after a tray accepts a report it repeats it at most every five minutes per agent and checkout. It waits at most 500 ms for that check and sends the report only over the tray's local activity socket. The tray deduplicates by main checkout, keeps at most eight, forgets them after an hour, and forgets a checkout once it is connected, so removing that connection later does not bring the row back until an agent runs there again. Connect never starts a new sign-in; without a saved sign-in, **Copy Terminal Setup** copies `agentscm quickstart` for the checkout.
- Fixed: hooks, the client MCP server and Hub-backed CLI commands started in a linked Git worktree of a connected checkout now use that checkout's connection, so sessions that Claude Code or Codex start in a new worktree coordinate without running setup again. Edit paths are reported relative to the worktree. Separate or nested clones, submodules and copied `.git` files still never inherit a connection. With no connection, the hook note now names `agentscm quickstart`.

- Upgrade the Hub before clients: the new change-record event fields and kinds are rejected by an older Hub, so envelopes from an upgraded client stay pending and metadata for a Work that ends before the Hub is upgraded is lost.

- Shared-conversation redaction also recognises URL passwords, secret-named CLI flags, `curl -u`, `mysql -pVALUE`, hash-rocket assignments, npm, PyPI and Hugging Face tokens and more secret-style field names, and replaces a secret-named JSON field's whole object or array. A shared-mode history import with many redacted bodies is split so no batch exceeds the gap limit, and the client skips bodies the Hub already holds.

- Experimental, Unix/Claude Code: opt-in shared agent conversations. A checkout in the new `shared-conversation` capture mode (default stays `metadata-only`) scrubs selected prompt, assistant and tool bodies of well-known credential shapes and secret-named fields on the client (best effort, not a guarantee; a `redacted` gap marks altered bodies, and bodies over 1 MiB stay local), then uploads them to the Hub after it accepts the event. Only the person who shared a body (through any of their tokens) and Hub admins can read or delete it, and a listing shows a caller their own entries; the Work owner has no special access (HTTP `.../change-records/{work_id}/conversation[/{sha256}]`, MCP `agentscm_shared_conversation`, `agentscm change-capture conversation`, and a read-only list in the Hub UI); other callers get 403. Bodies expire after 30 days and the author or an admin can delete one earlier through the CLI or HTTP (the Hub UI has no delete control). Change a checkout's mode with `agentscm change-capture set-mode` or `configure --capture-mode`, or with the new per-checkout **Share agent conversations with Hub** switch in the macOS tray (native UI not yet accepted by a human). A named Hub migration adds the shared-body table. See [sharing conversations](docs/change-capture.md#sharing-conversations-with-the-hub).

- Downgrading the client after this release is unsupported: older binaries cannot read the new session association records and fail on them.

- The `shared_conversations` Hub migration changed during development. A dev or staging Hub that ran a pre-release build of this branch must drop `hub_change_shared_bodies` and its named-migration record, or start fresh.

- New `agentscm hub serve --shared-conversation-retention-days N` (default 30, 1 to 3650) sets how long an uploaded shared body stays readable; a sweep every ten minutes removes expired bodies in batches. It is one global setting, not a per-team policy.

- The local capture config is written as version 6 only while some checkout is in `shared-conversation` mode; an older binary refuses a version 6 file. Without an opt-in the config stays at version 5.

- Change-record capture records more: the host tool name and whether a finished tool call `returned` or `failed`, session start and end boundaries, turn-ended (`Stop`), compaction (`PreCompact`) and subagent start and stop events, now installed as eleven hook groups. The new markers are informational and never raise coverage. These hooks are tested with synthetic fixtures, not live Claude Code payloads.

- The change record lists per-session model, effort and permission mode (`session_setups`, up to 16 entries with a `parent_session_id`) when a session reported them.

- Changed: the Hub no longer reads the start prompt of a `SessionStart` hook (including Copilot's `initialPrompt`) and so no longer stores it as a team-visible task.

## 0.1.0-alpha.6 (2026-10-25)

- 0.1.0-alpha.5 was not released: its Linux musl binaries failed to link after its container image was pushed, so it has no GitHub release, npm package or attestations. Use 0.1.0-alpha.6, which contains everything listed below.

- Fixed: the static Linux (musl) binaries link again. Local change capture's no-replace export rename now calls the `renameat2` system call directly instead of a libc wrapper that musl does not provide.

- Optional macOS menu-bar companion in the existing binary: `npx agentscm tray`, or `--tray` after successful quickstart setup. It shows saved checkout connections, recent Hub checks and observed hook/MCP activity, configures coding-agent integrations with visible results, and opens a short-lived read-only Hub session. Start at Login is opt-in and its owned setting is removed when uninstall removes the program. The tray is an unsigned, unnotarized alpha; native UI and OS login/logout acceptance remain incomplete. See [install options](docs/install.md#where-files-go).

- Fixed: client setup and connection release the shared lifecycle lock while waiting for Hub sign-in or repository access, then recheck local state before applying changes. Tray recovery commands use the running executable's full path. Platform-specific activity imports no longer break Windows test builds.

- Hub UI: a feature walkthrough with per-page tours, search pickers instead of pasted IDs, one shared pager that keeps its cursor in the URL, and a top bar and rail set apart from page content.

- `agentscm init-repo` now writes only the shared agent instructions (AGENTS.md and the files that point each agent at it), with no hook, MCP server or token. Each developer connects a checkout with `agentscm client setup`, the command the instructions name.

- Experimental, Unix, GitHub App connections, fake-tested only: the opt-in `agentscm/workflow/<target>` status is also posted on GitHub merge queue groups, `success` only when the queue branch still points at the group commit, the queued pull request's head is the published candidate, the group's base is the candidate baseline and the group tree equals the qualified native revision's tree. Stacked multi-entry groups stay pending. Named Hub migration `merge_groups`; the publication API adds `forge_check.merge_queue`. See [GitHub merge queues](docs/advanced/github-merge-queue.md).

- `agentscm setup` is now a hidden contributor helper; new users run `agentscm quickstart`. Its guide moved to [native installation](docs/advanced/native-installation.md), and the install guide asks first whether the hub is local or [hosted](docs/hosted.md).

- Update `tokio` to 1.53.3 and `ctutils` to 0.4.5 (RUSTSEC-2026-0385 and RUSTSEC-2026-0386).

- The Hub UI gains Landed changes, Templates and Timeline pages; Templates and Timeline read a local `agentscm serve` through its `/v1/local` routes.

- Experimental: work insights. `GET /v1/hub/standup/insights` and `/v1/hub/standup/work/{id}/insights`, MCP `agentscm_work_insights` and `agentscm_work_insight`, and `agentscm work insights|insight` report bounded timing, coordination signals, usage reports and stored publication observations; `agentscm work usage` and MCP `agentscm_work_usage` record immutable usage reports, and `agentscm client reporting on|off` controls completion reporting (off by default). See [work insights](docs/work-insights.md).

- Experimental, Unix: local workspace snapshot timelines. `workspace capture|timeline|entry|diff` record and inspect bounded observations of a native workspace without checkpointing it, an earlier observation can be forked into another Change, and `workspace-client mcp` exposes the same operations as a local stdio MCP server. See [workspace timeline](docs/workspace-timeline.md).

- Experimental: reusable skill workflow templates. `skill-workflow library inspect` and, on Unix, `library configure` bind one strict versioned manifest with a packaged `SKILL.md`, stored in immutable native source, to a Change's exact revision and review context; `skill-workflow mcp` exposes them as a local stdio MCP server. There is no mutable catalog or `latest`. See [reusable workflows](docs/advanced/reusable-workflows.md).

- Hub listener: requests other than workspace source publication end with `408` after 30 seconds, and one client address may hold at most 8 pending browser and 8 pending device sign-ins (`429 too_many_logins`). Behind a loopback or private-network proxy the last `X-Forwarded-For` entry is used as the address.

- Experimental: supported skills. `agentscm skill list|show|add|remove` browses a catalog of versioned workflows and pins them in a committed `agentscm-skills.lock.json`. The first skill, `pr-gate` v1, runs on any GitHub or GitLab repository (branch, tests, independent review, fixes, CI, approval, merge, post-merge check), with parameters for the CI label, merge method and stacked pull requests. Agents record self-reported evidence per gate (`agentscm skill evidence`, MCP `agentscm_skill_evidence`), and every gate it satisfies is labelled self-reported in the CLI, MCP and Hub. The opt-in `agentscm skill stop` Stop hook keeps an author session from finishing at the tests, review and fix gates. The Hub reads the lock from the default branch when a forge is connected and shows a Skills library and per-branch runs. See the [design](docs/design/supported-skills.md).

- `agentscm quickstart` now sets up coding agents through `agentscm client setup` instead of writing its own Claude Code hooks, token file and `hub.json`. `--agent claude|codex|cursor|copilot|gemini` (repeatable) and `--no-agents` replace `--hook`, `--ttl` and `--agent-name`. Agents set up by quickstart appear on the Connect agents page. Client tokens last 30 days (was 8 hours). `client connect` and `client setup` wait at a terminal for the hub's GitHub App to be installed on the repository. `client remove` now undoes `client setup` fully (entries, plugin, connection, token file, token), and `agentscm uninstall` runs it for each checkout quickstart connected. `agentscm intent` reads the checkout's client connection before `hub.json`. With `--yes` or no terminal, quickstart never starts a sign-in; with no agent found, hub setup still succeeds and prints each agent's `client setup` line. `uninstall` leaves a connection quickstart found rather than made. New `DELETE /v1/hub/token` lets an agent token revoke itself; `client remove` uses it and exits non-zero, keeping the connection for a retry, when the token can't be revoked. See the [setup guide](docs/setup-guide.md).

- Optional Slack app (`AGENTSCM_SLACK_FILE`, `agentscm hub slack ...`, `quickstart --slack-config-file`): a direct message to each person involved when an agent edits a file again after AgentSCM refused it (`edit_repeated`), when a handoff is declined, and when new work reads like work already under way (`task_overlap`). The holder's message has buttons that answer the handoff for their own agent. Bot scope `chat:write` only; no channels, history, Home tab, slash command or Slack-side work forms. Handoff answer events now carry the requester's token in `from.token`. New store method `latest_event_seq`.
- Experimental, Unix/Claude Code: add configure-once local change capture
  with a bounded private spool, current-Work resolution on each hook, replay-safe Hub event/action
  receipts and typed Git/native source bindings. Ordinary execution outcome is separate from an
  exact-recipe verification result. Read one concise transcript-free intent/source/decision/next
  action handoff through HTTP, `agentscm_change_record`, `agentscm change-capture show` or the Hub
  UI. Existing Work versions retain declared intent/decision provenance; bounded observed setup
  facts and opt-in, session-bound Claude history reads supply partial usage coverage through Work
  Insights. Request usage and session totals remain separate. Conversation/tool bodies, captured
  source bytes and command output stay local. See [local change capture](docs/change-capture.md)
  and [qualification limits](docs/implementation/agentscm-build-status.md).

- Fixed: an agent whose hook refuses contested edits (Claude Code, Codex, Copilot) was never refused when it had first named the file in `agentscm_start` or `agentscm_check`, because that MCP call already held the file and only new files were checked ([#83](https://github.com/counterbranch/agentscm/issues/83)). The hub now records which held files no refusing edit has checked yet and checks each on the session's first refusing edit, in either order of MCP call and first hook. Hub migration 20 adds that list to the session row. See [warn once](docs/hub.md#warn-once).

- CI: ordinary same-repository pull request updates no longer run the hosted test suites. `CI` is split into `static` (always) and `tests`, and `tests`, Windows, container, hub end-to-end and live execution run for same-repository pull requests only with the `ci-extended` label or `workflow_dispatch`; forks, pushes to `main` and the security checks are unchanged. See [local validation](docs/advanced/local-validation.md#hosted-ci-and-required-checks).

- Experimental, Unix and GitHub only: `agentscm local-validation run|show|post|verify` records a maintainer's local gate as a signed attestation; the repository now carries `.github/local-validation/` (recipe, policy and an issuer registry with one enrolled maintainer key) and a `local-validation` workflow that verifies signed local validation attestations posted as PR comments and publishes an `agentscm/local-validation` commit status on the exact head, without building PR code. The verifier was qualified before merge by running this workflow under `act` against live GitHub (pull request #86: a signed failure, then a fix and a signed pass, each with a status on its exact head); the first run on GitHub-hosted Actions happens after merge. The status is not a required check. See [local validation](docs/advanced/local-validation.md).

- When a pull request is merged, every live intent overlapping its files (other than the pull request's own branch) gets a `pr_notice` with `"merged": true`, so an agent still editing them is told mid-task to merge or rebase onto the base first. An `agentscm_start` that creates new work now warns when its task reads like another agent's ongoing work or an open pull request and tells the agent to ask the user, recording a `task_overlap` event. The status page's activity shows merged notices and similar work. The store method `append_pr_notices` is now `append_notices(kind, ...)`.

- Read-only tokens (the status page's) can now read publications. `GET /v1/hub/repos/{repo}/publications/{id}` accepts them and returns the stored row: for those tokens it never reads the forge or records a merge. A new `GET /v1/hub/repos/{repo}/publications?limit=&before=` lists a repository's publications newest first, with `next_cursor` and `complete`. It reads the store only and omits `forge_check` and `external`. See [publishing a revision](docs/hub.md#publishing-a-revision).

- Experimental, Unix for the timeline: `agentscm serve` gains read-only `/v1/local` routes that return the JSON of `skill-workflow library inspect` and `handoff`, and of `workspace timeline`, `entry` and `diff`, with the CLI's paging and bounds. The timeline is pinned with `--timeline-workspace ROOT`. Setting `AGENTSCM_SERVE_READ_TOKEN` requires a bearer token on these routes. `--browser-origin ORIGIN` adds exact-origin CORS and requires the token. A non-loopback `Host` gets 421. Capture, fork, configure and check stay local. See [local template and timeline reads](docs/api.md#local-template-and-timeline-reads).

- The hub UI at `/ui/` is now the redesigned Bootstrap UI (option 1c); the Ant Design UI is removed.

- Experimental, fake-tested only (no real agent host verified): the Hub MCP server can offer two read-only inline views (`ui://agentscm/work-overview`, `ui://agentscm/coordination-inbox`, MCP Apps `io.modelcontextprotocol/ui`) to hosts that support them; other hosts keep the same text results. Each user can turn all views or each view on or off for their own agents (a per-user override over an administrator-set hub default; Hub migration 18), via the writer-only `agentscm_view_settings` tool, `GET /v1/hub/view-settings`, `PUT /v1/hub/workspace/view-settings/me` (any signed-in member), `PUT /v1/hub/workspace/view-settings` (workspace administrator, the hub default) and the Settings page of the hub UI. Views never acknowledge, reply or write; overview and inbox results gain one text line naming the settings tool. Layout and dark mode are not checked in a browser.

- Experimental, off by default, fake-tested only (in-process fake GitHub; REST shapes not live-verified): `AGENTSCM_GITHUB_EXTERNAL_RUNS` (comma-separated `owner/name`) makes the Hub poll GitHub Actions push runs on the base branch and deployments with their newest status for the exact merge commit of merged publications (Hub migration 17). `GET` of a publication gains an `external` object with `ci` and `deployments` kept apart from source approval and `forge_check`, and a `state` of `unobserved`, `incomplete` or `observed`; missing data is never shown as success. Observation only: no logs, reruns, dispatches or gating, and the token needs `actions: read` and `deployments: read` (the GitHub App manifest does not request them yet). GitLab pipelines, Jenkins and webhook hints are unsupported. See [external outcomes](docs/hub.md#external-ci-and-deployment-outcomes).

- A leased remote push that the server conclusively refuses (one `!` line for the target ref marked `[remote rejected]` or `[rejected]`, with the expected preimage still in place on re-read) is now recorded as a terminal, effect-free `rejected` state with a closed `rejection` kind instead of `unknown`. It does not fence new operations; `publish-branch` reports `ok:false` and `publish-pr` refuses it. The kind comes from Git/server summary text only (nothing is stored), so `protected_branch` appears only when the server says so; GitLab may report just `hook_declined`. Tested with local bare remotes and loopback HTTPS, not live forges. The journal version is unchanged, so an older binary fails closed on a journal containing a `rejected` row.
- Experimental: attached-worktree candidate publication (`publish-pr`, `recover-pr`, Hub candidate manifests) now creates or adopts a GitLab merge request on a GitLab PAT connection, including nested groups. It is tested only against an in-process fake GitLab with real local Git and loopback HTTPS, not live or self-managed GitLab. A merge without a merge commit (fast-forward or merged by push) is recorded only when the MR head equals the published commit. Fork MRs are never adopted; a truncated search or 429 blocks without creating; on create 400/403/404/422 is a GitLab no-create refusal (403/404/422 on GitHub; a GitHub 400 stays uncertain) while 409, 5xx and transport failures stay uncertain. Event publication (`hub publish --event`) remains GitHub-only; for the opt-in workflow status see the GitLab entry below; create returns 409 `ambiguous_repository_name` if the name is connected on another forge host; a relative URL root is unsupported; a conclusive push refusal is recorded as `rejected` (the kind mapping is unverified against live GitLab, which may report only `hook_declined`). See the [live qualification checklist](docs/advanced/attached-git-worktrees.md#live-gitlab-qualification-checklist-opt-in).
- Experimental, off by default: an agent-task profile with `publish_forge_status: true` makes the Hub post a GitHub commit status `agentscm/workflow/<target>` on a candidate pull request, `success` only when an operator-pinned read-only native store on the Hub host shows the exact candidate currently qualified, the head tree and parents match the native revision and the base tip equals the candidate baseline. Posts are journalled (Hub migration 16) and run on poll and `pull_request`/`push` webhooks. `GET` of a candidate publication gains a read-only `forge_check` object with `protection: "unverified"`. AgentSCM does not create or verify branch protection; the operator must require the context, ideally pinned to the posting App, and merge queue is unsupported. Unix only, tested against fakes only; live verification and signed attestations for laptop-only stores are not implemented. GitLab is covered in the next entry. See [workflow status](docs/hub.md#workflow-status-for-candidate-pull-requests).
- Experimental, fake-tested only (in-process fake GitLab; no live GitLab verification): `publish_forge_status` also makes a GitLab connection post `agentscm/workflow/<target>` on candidate merge requests, with the same pending, failure, error, success and retire states (`pending`, `success`, `failed` on GitLab). `success` needs an open MR in the connected project (fork MRs are ignored), the candidate target and baseline, the MR `sha` equal to the source branch tip and published commit, the expected parents, a root tree rebuilt from a bounded `repository/tree` listing (5 pages) equal to the native tree, and exact qualification in the pinned store. The status is posted on the source branch ref and journalled in the existing migration 16 table. A lost reply, or a pending "cannot transition" 400, is resolved by reading the commit's status list (2 pages), and the status is posted again only when the list shows the desired status absent; 429 stops the pass; 403/404 and a 400 for any other state are recorded as refused. A fast-forward merge is recorded when the merged head equals the published commit. Poll and verified `Merge Request Hook`/`Push Hook` webhooks trigger it; `forge_check.provider` is `gitlab` with `protection: "unverified"`. A GitLab status on the MR head is not a verified merged-result pipeline, merge-train or "pipelines must succeed" result, may join or create a pipeline for that ref, and any Developer-role user can post the same name; required-status configuration is operator-owned and unverified, and AgentSCM never changes protections, approves or merges. Event publication stays GitHub-only. See [GitLab merge requests](docs/hub.md#gitlab-merge-requests).

## 0.1.0-alpha.4 (2026-10-21)

- Experimental Unix-only attached Git worktrees can capture and qualify exact native Changes, then publish a separate branch and GitHub PR through an owned mirror. Qualification uses synthetic checkouts, real local Git, loopback authenticated HTTPS and fake GitHub; live forge is not qualified and attached SSH identities are unsupported. Hub migration 15 preserves integrated publications but is not downgrade-safe for older binaries reading candidate rows. See [attached Git worktrees](docs/advanced/attached-git-worktrees.md) for unsupported operations and recovery limits.
- Update `rustls-webpki` to 0.103.16 and `webpki-roots` to 1.0.10 to reject distrusted emSign intermediates (RUSTSEC-2026-0382 and RUSTSEC-2026-0381).

- Live CLI qualification of the local Stop hooks (2026-10-20, one macOS machine, synthetic fixture, local native store). Claude Code CLI 2.1.304 print mode: `claude-stop` blocked with the failing test's output, passed after the fix with a current receipt confirmed independently, and an always-failing requirement stopped after 8 blocks. Codex CLI 0.163.0 `codex exec`: `codex-stop` blocked once and then passed, but hooks fired only with `--dangerously-bypass-hook-trust` and a native workspace also needs `--skip-git-repo-check`; the bound was not live-tested on Codex. Interactive sessions, desktop/IDE surfaces, hosted `remote-*` stops and agent-owned `task-*` stops are not live-tested. The capability matrix is now version 2 and notes vendor documentation for Cursor (`stop` can only continue via `followup_message`), Copilot (`agentStop` blocking, documented, not verified) and VS Code agent hooks (preview). See the [capability matrix](docs/agents.md#workflow-lifecycle-capability-matrix).
- Thin instruction entrypoint: `init-repo`'s managed block gains a short "Workflow tasks" section (discover with `capabilities`, fetch one `reference` on demand, never run feedback as a command), `schema_mismatch` from `instructions` and `reference` adds `server_schema_versions` and a fixed `action`, and `install-stop` task modes print an `instructions` pointer. No background updater is added.
- `skill-workflow task-stop` and `task-codex-stop`: read-only Stop hooks for agent-owned tasks. They call the facade `qualification` operation (30 second limit) and pass only when it is current and qualified and the clean native workspace base is the qualified revision; otherwise, including on network, auth or malformed-response errors, they block with typed failures and `next` data and never claim, collect, upload, finalize or checkpoint. `install-stop` accepts both. Fixture-tested, not live-qualified. See [agent workflow tasks](docs/advanced/agent-workflow-tasks.md#stop-hook-agent-owned-mode).
- Agent-task `capabilities` (schema discovery) and read-only `qualification` (is this exact subject currently qualified, with typed `next` data) over the workspace HTTP and MCP facade. A `reference` request with an unsupported `expected_schema_version` now returns a typed `schema_mismatch` instead of a generic invalid-request error.
- Stop triggers for Codex CLI and a bounded, installable Stop loop. `skill-workflow codex-stop` and `remote-codex-stop` are Codex CLI Stop hooks with the pass/block behavior of `claude-stop` and `remote-stop`. All four share a handler that reads at most 64 KiB, requires a `Stop` event, tolerates vendor fields, and allows at most 8 consecutive automatic continuations per agent, session, workspace and change (a runaway-loop guard, not crash-durable; counted under `<data-dir>/agent-stop/`; `stop_hook_active` is required and an explicit `false` resets the count and a pass does not; once exhausted or unreadable it allows the stop and says nothing was qualified). A payload `cwd` disjoint from `--workspace` is a no-op, and Stop subcommand parse errors exit 1 so they do not block. The local Stop pass predicate is tightened (`workspace_changed_during_run` must be false) and local concurrent-checkpoint detection is fixed (generation, base revision and pending operation are compared between the state and status reads). `skill-workflow install-stop` previews by default and, with `--apply`, adds or removes only its own entry for the same workspace (`--remove [--workspace PATH]`) in an operator-local Claude Code or Codex hook file, refusing files inside the workspace and symbolic links. The hooks are tested with fixture payloads built from vendor documentation, not live-qualified; omitting or bypassing them does not bypass native landing policy. See [Stop hook guide](docs/advanced/claude-code-workflows.md#opt-in-claude-code-stop-hook) and the [capability matrix](docs/agents.md#workflow-lifecycle-capability-matrix).
- Add staged product usage telemetry with inspectable coarse feature/day events, a pseudonymous random installation identifier, persistent on/off controls and environment or per-command overrides. Transmission remains inactive without a configured HTTPS endpoint and data-handling profile; consent-required profiles wait for an affirmative choice. No production endpoint is configured. See [telemetry](docs/telemetry.md).
- Workspace capture honors exclusion rules. Status, checkpoint and the other workspace scans leave out untracked paths matched by `.gitignore`, a root `.agentscmignore`, the private `.agentscm-workspace/exclude` and, with a root `.git`, Git's global and `info/exclude` rules; base-revision paths are always captured. A root `.git` is skipped as metadata but needs a working `git` that accepts the repository. `agentscm workspace preview` lists included and excluded paths with the deciding rule, and `workspace status` reports `excluded_total`. Undo, merge application and refresh block rather than overwrite an excluded local file. Local backup still copies ignored files. See [source selection](docs/advanced/workspace.md#source-selection).
- A hosted laptop check counts as changed if the workspace base moves during the run.

## 0.1.0-alpha.3 (2026-10-17)

- `agentscm demo` ends with the next step, `agentscm quickstart` (or `npx agentscm`), and the status page's demo banner says the same.

## 0.1.0-alpha.2 (2026-10-17)

- `npx agentscm --version` and `--help` reach the binary instead of the setup guide.

## 0.1.0-alpha.1 (2026-10-17)

First alpha. **Not for production.** No scale, high-availability or security audit yet.

### What is in it

- **Collision warnings.** `agentscm collisions` and `GET /v1/collisions` report which branches and worktrees touch the same files, with a `git merge-tree` conflict check; read-only. See [collisions](docs/collisions.md).
- **Autopsy.** `agentscm autopsy` counts, from your own Claude Code logs, how often two sessions edited the same file while one still held it; local and read-only. See [autopsy](docs/autopsy.md).
- **Merged branches skipped.** `agentscm collisions` skips branches the default branch already has through a squash or rebase merge; `--include-merged` restores the old scan.
- **Readable output.** On a terminal `agentscm collisions` prints a short summary; `--json` and `--text` choose the format, and `--git-dir` defaults to the current directory.
- **Coordination hub.** `agentscm hub serve` on SQLite or PostgreSQL with local or S3-compatible storage, GitHub or GitLab sign-in, expiring agent tokens and an append-only event log. See [hub](docs/hub.md).
- **Work intents.** Agents announce the files they are about to edit and get attributed notices when others overlap, through the hub API, `agentscm intent` and a Claude Code hook example. See [work intents](docs/intents.md).
- **Warn once.** On Claude Code, Codex and Copilot pre-edit hooks the hub refuses the first edit in a session to a file another session holds; a repeat passes and nothing is locked. See [warn once](docs/hub.md#warn-once).
- **MCP and hook endpoints.** `POST /mcp` (six tools) and `POST /v1/hub/hook/{claude,codex,cursor,copilot,generic}` give agents the same coordination with nothing installed; the hook endpoints always answer 200 and fail open. See [agent sessions](docs/hub.md#agent-sessions-mcp-and-hook-endpoints).
- **Handoff (experimental).** A refusal asks the holder to hand the file over, and the holder answers with `agentscm_handoff`. See [handoff](docs/hub.md#handoff).
- **Status page.** The hub serves a read-only page at `/ui/` (in the container image and release binaries) showing live agents, contested files, overlapping pull requests, recent activity and handoffs; it signs in only with a read-only token (`agentscm hub token create --read-only`). See [status page](docs/hub.md#status-page).
- **Demo.** `agentscm demo` (or `npx agentscm demo`) runs a throwaway hub with three simulated agents in any Git repository and opens the status page; the agents and the handoff grant are scripted. See [the demo](docs/hub.md#try-it-locally-agentscm-demo).
- **Forge sync.** The hub observes open pull and merge requests and recent branches on GitHub and GitLab, and can post an opt-in comment and commit status. Tested only against in-process fakes.
- **Publish to GitHub.** `agentscm hub publish` turns a native revision in an exported mirror into a branch and pull request, resumably and without forcing.
- **Agent wiring.** `agentscm init-repo` writes committed config for Claude Code, Codex, Cursor and Copilot in VS Code, plus `AGENTS.md` and Gemini's pointer file; `agentscm hook <agent>` is the thin client for command hooks and fails open. See [supported agents](docs/agents.md).
- **Guided setup.** `npx agentscm` / `agentscm quickstart` walks through a first install; `agentscm uninstall` undoes it.
- **Native ledger.** Immutable revisions, review contexts, approvals, conditional integration, workspaces, merges and offline exchange. Experimental; see [advanced](docs/advanced/README.md).
- **Isolated command checks.** Run a declared command against an exact revision in a locked-down container and bind the result to approval. Experimental; see [execution](docs/advanced/execution.md).
- **Git bridges.** Snapshot capture, reconcile and export between Git and the native ledger, for a local shadow workflow. Experimental; see [advanced](docs/advanced/README.md).
- **Compose.** A single-host trial profile and a PostgreSQL profile (`postgres`, service `hub-postgres`; a volume from the old profile name is not reused). See [hub](docs/hub.md#running-in-containers).
- **Windows x86_64 binary.** Checksummed and attested like the others, installed by `npx agentscm`, with a subset of commands. See [install](docs/install.md#windows).
- **Container image and docs.** The release workflow publishes `ghcr.io/counterbranch/agentscm:<version>`, and CI covers formatting, clippy, tests, a container smoke test and a two-client hub end-to-end test. See [deployment](docs/deployment.md).

### Known limits

- Nothing is locked. Intents and collision warnings are advice; the only refusal is the warn-once refusal of one edit, and a repeat passes. Notices are collected by polling, not pushed, and none arrives when an intent expires. Revoked or expired tokens do not release their intents.
- Overlap means the same path or a path under an announced `dir/**`. Semantic conflicts are not detected, and work that is not announced is invisible. Only edit and write tool calls are announced, not shell writes.
- Hub sessions live in memory: a restart signs everyone out.
- The hub does not terminate TLS; put it behind a proxy. Plain `http` is accepted only for loopback addresses. The PostgreSQL connection has no TLS.
- GitHub and GitLab sync and GitHub publishing have been tested only against in-process fakes, not live forges. GitLab merge requests cannot be created. Publishing handles one parent, at most 1,000 paths and 64 MiB.
- Command execution is not a sandbox for arbitrary hostile code. The container-boundary tests qualify only the runtime they run on; there is no unattended recovery after a controller crash, and records are unsigned. Linux and Unix hosts with Docker only.
- Git export and reconcile refuse executable and symlink entries. Snapshot capture is one commit at a time with no history import. The hub is not yet joined to the native ledger, so there is no multi-user landing admission and no cutover from Git.
- One writable authority per data directory; the hub uses one serialized database connection per process. Nothing is measured at tens or hundreds of agents, and overlap lookup was measured only on a synthetic 1,000 intents.
- The read-only `serve` API is unauthenticated and meant for loopback; `serve` refuses a non-loopback `--bind` without `--public`, and exposing it needs an authenticating proxy you operate.
- Claude Code is the reference integration; the Codex, Cursor and Copilot hooks follow their vendors' documented formats. Claude Code cannot use an HTTP hook for `SessionStart`, and Cursor's pre-edit hook cannot carry a warning, so Cursor is warned just after the edit. Windows is not yet supported by `quickstart`.
- Schemas and storage may change between alpha releases without migration, and the authority database cannot be downgraded.
- The Windows binary is not code-signed, and the macOS binaries are not notarized. The Windows binary supports a subset of commands (setup guide without the collision check, `hook`, `init-repo`, hub client and hub, intents; not `collisions`, the native ledger, Git bridge, command checks, team service or recovery commands) and is x86_64 only.
- No automated model review.
