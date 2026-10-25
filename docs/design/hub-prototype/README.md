# Hub dashboard prototype (local preview)

A clickable design prototype for the Hub UI. It is not the shipped Hub (that lives in `ui/`), and every name, repository and number in it is invented.

```
node docs/design/hub-prototype/server.mjs        # then open http://127.0.0.1:5210/
```

The server opens with the team story already played through: six people in six time zones sharing one repository. It then keeps sending new, random but valid events every few seconds: agent check-ins, check runs and commits on the open pull request, and an occasional heads-up that gets answered and settles. Every open tab shows the same moment. There are no playback controls.

## Pages

Home is one **Dashboard**. A sentence says what is happening, then four tabs each answer one question and show a live status:

- `#/home/now`: **Now**. What needs you, then the track: every task as a lane bar with its holder, start check, next step and its agent's last words.
- `#/home/where`: **Where**. The repository as a map of areas. Faces show who plans to work where; a crossing marks two agents planning the same file.
- `#/home/catchup`: **Catch up**. "Since you last checked" (a sentence, three lanes of dots, per-source coverage and retention notes), the full log and baton passes.
- `#/home/team`: **Team**. Who is awake and who ran when, the crew and today's tallies.

Other pages: Inbox, History (Sessions and Files), Timeline (saved moments), Skills (Skills, Checks, Recipes), Insights, Setup (Connection, Agents, Access), and a page per task and per session. The `?` in the top bar explains the current page in three steps. Features that are not implemented in the Hub carry a **Proposed** tag.

The page never runs anything: answers in the Inbox are queued for the agent's next check-in, and commands are copied for an agent or the CLI.

## Files

- `server.mjs`: serves `public/` and streams events over server-sent events.
- `scenarios.mjs`: the two scripted stories (solo developer and team of six).
- `live.mjs`: the random live events after the scripted day ends.
- `public/core.js` and `public/dashboard.js`: the reducer and every page.
- `public/hub.css` and `public/skins.css`: styles. Twilight is the chosen skin; the others remain for comparison at `/themes.html`.
- `DESIGN.md`: the design system the prototype uses.
- `selfcheck.mjs`: `node docs/design/hub-prototype/selfcheck.mjs` replays both stories, renders every page at each step, and runs 400 live events per story through the reducer.

`public/vendor/` holds Bootstrap 5.3.8 and Bootstrap Icons 1.13.2, both MIT-licensed, with their license headers intact. Fonts load from Google Fonts.
