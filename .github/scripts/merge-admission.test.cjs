const {test} = require('node:test');
const assert = require('node:assert/strict');
const {admit, pages} = require('./merge-admission.cjs');

function fixture() {
  const pull = {number: 7, state: 'open', head: {sha: 'a'.repeat(40)}, base: {ref: 'main'}, title: 'fix: Example'};
  const statuses = [{context: 'agentscm/local-validation', state: 'success', creator: {id: 41898282}}];
  const checks = [
    {id: 4, name: 'Conventional PR title', status: 'completed', conclusion: 'success', app: {id: 15368}},
    ...['static', 'tests'].map((name, i) => ({id: 10 + i, name, head_sha: pull.head.sha, status: 'completed', conclusion: 'success', app: {id: 15368}})),
  ];
  const writes = [];
  const statusWrites = [];
  let reads = 0;
  const f = {pull, statuses, checks, writes, statusWrites};
  f.args = {
    context: {repo: {owner: 'synthetic', repo: 'example'}, payload: {repository: {default_branch: 'main'}, pull_request: pull}},
    validateTitle: title => title.startsWith('fix: '),
    github: {rest: {
      pulls: {
        get: async () => ({data: {...pull, head: {sha: ++reads === 2 && f.moved ? 'b'.repeat(40) : pull.head.sha}}}),
        list: async () => ({data: [pull]}),
      },
      repos: {
        listCommitStatusesForRef: async () => {
          if (f.apiError) throw Error('unavailable');
          return {data: statuses};
        },
        createCommitStatus: async value => {
          statusWrites.push(value);
          statuses.unshift({context: value.context, state: value.state, description: value.description, creator: {id: 41898282}});
        },
      },
      checks: {
        listForRef: async ({ref}) => ({data: {check_runs: checks.filter(c => !c.head_sha || c.head_sha === ref)}}),
        create: async value => {
          writes.push(value);
          const run = {id: 90 + writes.length, name: value.name, head_sha: value.head_sha, status: value.status, app: {id: 15368}};
          checks.push(run);
          return {data: run};
        },
        update: async value => {writes.push(value);},
      },
    }},
  };
  return f;
}

test('exact trusted success admits only the PR head', async () => {
  const f = fixture();
  await admit(f.args);
  assert.equal(f.writes[0].head_sha, f.pull.head.sha);
  assert.equal(f.writes[1].conclusion, 'success');
  assert.match(f.writes[1].output.title, /batch tests still required/);
});

test('own event reports pending, then the outcome, as a commit status on the head', async () => {
  const f = fixture();
  await admit(f.args);
  assert.deepEqual(f.statusWrites.map(s => [s.sha, s.context, s.state, s.description]), [
    [f.pull.head.sha, 'Merge batch gate', 'pending', 'PR admission pending'],
    [f.pull.head.sha, 'Merge batch gate', 'success', 'PR admitted; batch tests still required'],
  ]);
  const g = fixture(); g.pull.title = 'invalid title';
  await admit(g.args);
  assert.equal(g.statusWrites.at(-1).state, 'failure');
  assert.equal(g.statusWrites.at(-1).description, 'PR admission blocked: PR title failed');
});

const ci = (f, name) => f.checks.find(c => c.name === name);

test('both CI checks green on the exact head admit', async () => {
  const f = fixture();
  await admit(f.args);
  assert.equal(f.writes[1].conclusion, 'success');
  assert.match(f.writes[1].output.summary, /static and tests must pass on this head/);
});

test('a queued or running CI check keeps admission pending and says what it waits for', async () => {
  for (const status of ['queued', 'in_progress']) {
    const f = fixture(); Object.assign(ci(f, 'tests'), {status, conclusion: null});
    await admit(f.args);
    assert.equal(f.writes[1].status, 'in_progress');
    assert.equal(f.writes[1].conclusion, undefined);
    assert.equal(f.writes[1].output.title, 'PR admission pending: waiting for tests');
    assert.deepEqual(f.statusWrites.at(-1).state, 'pending');
  }
});

test('a failed CI check refuses admission and names it', async () => {
  for (const conclusion of ['failure', 'cancelled', 'skipped']) {
    const f = fixture(); ci(f, 'static').conclusion = conclusion;
    Object.assign(ci(f, 'tests'), {status: 'in_progress', conclusion: null});
    await admit(f.args);
    assert.equal(f.writes[1].conclusion, 'failure');
    assert.equal(f.statusWrites.at(-1).description, 'PR admission blocked: static failed');
  }
});

test('a same-named check from another app never counts', async () => {
  const f = fixture(); ci(f, 'tests').app.id = 1;
  await admit(f.args);
  assert.equal(f.writes[1].output.title, 'PR admission pending: waiting for tests');
  const g = fixture(); g.checks.push({...ci(g, 'static'), id: 20, conclusion: 'failure', app: {id: 1}});
  await admit(g.args);
  assert.equal(g.writes[1].conclusion, 'success');
});

test('the newest run of each CI check decides when a rerun left an older suite', async () => {
  // ready_for_review reruns CI on the same head and cancels the older suite's run.
  const f = fixture(); f.checks.push({...ci(f, 'tests'), id: 5, conclusion: 'cancelled'});
  await admit(f.args);
  assert.equal(f.writes[1].conclusion, 'success');
  const g = fixture(); g.checks.push({...ci(g, 'tests'), id: 30, conclusion: 'failure'});
  await admit(g.args);
  assert.equal(g.statusWrites.at(-1).description, 'PR admission blocked: tests failed');
});

test('a CI check on an older head never counts', async () => {
  // Mock-only: in production the real protection is listForRef({ref: head}).
  const f = fixture(); ci(f, 'static').head_sha = 'e'.repeat(40);
  await admit(f.args);
  assert.equal(f.writes[1].output.title, 'PR admission pending: waiting for static');
});

test('CI completion re-evaluates a pending pull request and admits it without a new PR event', async () => {
  const f = fixture();
  for (const name of ['static', 'tests']) Object.assign(ci(f, name), {status: 'in_progress', conclusion: null});
  await admit(f.args);
  assert.equal(f.writes[1].status, 'in_progress');
  for (const name of ['static', 'tests']) Object.assign(ci(f, name), {status: 'completed', conclusion: 'success'});
  delete f.args.context.payload.pull_request;
  f.args.context.payload.workflow_run = {id: 9, name: 'CI'};
  f.args.context.payload.action = 'completed';
  await admit(f.args);
  assert.equal(f.writes.at(-1).conclusion, 'success');
  assert.deepEqual(f.statusWrites.map(s => s.description), [
    'PR admission pending', 'PR admission pending: waiting for static, tests', 'PR admitted; batch tests still required',
  ]);
});

test('a failing status API never stops the check run from completing', async () => {
  const f = fixture();
  f.args.github.rest.repos.createCommitStatus = async () => { throw Error('statuses unavailable'); };
  await assert.rejects(admit(f.args), /Admission refresh failed/);
  assert.equal(f.writes[1].conclusion, 'success');
});

test('a sweep writes only a changed final status, never a pending one', async () => {
  const f = fixture(); delete f.args.context.payload.pull_request;
  await admit(f.args);
  await admit(f.args);
  assert.deepEqual(f.statusWrites.map(s => s.state), ['success']);
  f.checks[0].conclusion = 'failure';
  await admit(f.args);
  assert.deepEqual(f.statusWrites.map(s => s.state), ['success', 'failure']);
});

test('only a latest signed failure refuses; pending, error or no local validation admits', async () => {
  for (const state of ['pending', 'failure', 'error']) {
    const f = fixture();
    f.statuses.unshift({...f.statuses[0], state});
    await admit(f.args);
    assert.equal(f.writes[1].conclusion, state === 'failure' ? 'failure' : 'success');
    assert.match(f.writes[1].output.summary, new RegExp(`Signed local validation: ${state}`));
  }
  for (const alter of [f => f.statuses.splice(0), f => f.statuses[0].creator.id = 1]) {
    const f = fixture(); alter(f); f.statuses[0] && (f.statuses[0].state = 'failure');
    await admit(f.args);
    assert.equal(f.writes[1].conclusion, 'success');
    assert.match(f.writes[1].output.summary, /Signed local validation: none/);
  }
});

test('missing or wrong-identity title check never admits', async () => {
  for (const alter of [f => f.checks[0].app.id = 1, f => f.checks.splice(0)]) {
    const f = fixture(); alter(f);
    await admit(f.args);
    assert.equal(f.writes[1].status, 'in_progress');
  }
});

test('title edits and latest failed title check refuse admission', async () => {
  const f = fixture(); f.pull.title = 'invalid title';
  await admit(f.args);
  assert.equal(f.writes[1].conclusion, 'failure');
  const g = fixture(); g.checks.push({...g.checks[0], id: 5, conclusion: 'failure'});
  await admit(g.args);
  assert.equal(g.writes[1].conclusion, 'failure');
});

test('head movement leaves initial pending check; API failure invalidates first', async () => {
  const f = fixture(); f.moved = true;
  await admit(f.args);
  assert.equal(f.writes.length, 1);
  const g = fixture(); g.apiError = true;
  await assert.rejects(admit(g.args), /Admission refresh failed/);
  assert.equal(g.writes.length, 1);
  assert.equal(g.writes[0].status, 'in_progress');
});

test('workflow completion refreshes open PRs without candidate source', async () => {
  const f = fixture(); delete f.args.context.payload.pull_request;
  await admit(f.args);
  assert.equal(f.writes[1].conclusion, 'success');
});

test('pagination reads all pages and refuses incomplete coverage', async () => {
  const seen = [];
  const result = await pages(async ({page}) => {
    seen.push(page); return {data: page === 1 ? Array(100).fill('a') : ['b']};
  });
  assert.deepEqual(seen, [1, 2]); assert.equal(result.length, 101);
  await assert.rejects(pages(async () => ({data: Array(100).fill('a')})), /500/);
});

test('a prerequisite start leaves every gate as it is', async () => {
  for (const action of ['requested', 'in_progress']) {
    const f = fixture(); delete f.args.context.payload.pull_request;
    f.args.context.payload.workflow_run = {id: 3};
    f.args.context.payload.action = action;
    await admit(f.args);
    assert.equal(f.writes.length, 0);
    assert.equal(f.statusWrites.length, 0);
  }
});

test('one gate check run per head: later passes update it instead of adding another', async () => {
  const f = fixture();
  await admit(f.args);
  delete f.args.context.payload.pull_request;
  await admit(f.args);
  assert.equal(f.writes.filter(w => !w.check_run_id).length, 1);
  assert.ok(f.writes.filter(w => w.check_run_id).every(w => w.check_run_id === 91));
});

test('a sweep leaves an existing gate untouched until it writes the new outcome', async () => {
  const f = fixture();
  await admit(f.args);
  const before = f.writes.length;
  delete f.args.context.payload.pull_request;
  f.checks[0].conclusion = 'failure';
  await admit(f.args);
  const sweep = f.writes.slice(before);
  assert.equal(sweep.length, 1);
  assert.equal(sweep[0].check_run_id, 91);
  assert.equal(sweep[0].conclusion, 'failure');
});

test("the pull request's own event starts a fresh pending gate", async () => {
  const f = fixture();
  await admit(f.args);
  const before = f.writes.length;
  await admit(f.args);
  const own = f.writes.slice(before);
  assert.deepEqual(own.map(w => [w.check_run_id, w.status]), [[undefined, 'in_progress'], [93, 'completed']]);
});

test('a sweep never reuses a gate check run from another app', async () => {
  const f = fixture(); delete f.args.context.payload.pull_request;
  f.checks.push({id: 50, name: 'Merge batch gate', head_sha: f.pull.head.sha, status: 'completed', conclusion: 'success', app: {id: 1}});
  await admit(f.args);
  assert.equal(f.writes[0].head_sha, f.pull.head.sha);
  assert.equal(f.writes[0].check_run_id, undefined);
});


test('one PR API failure cannot preserve another PR old acceptance', async () => {
  const f = fixture(); delete f.args.context.payload.pull_request;
  const other = {...f.pull, number: 8, head: {sha: 'c'.repeat(40)}};
  for (const name of ['static', 'tests']) ci(f, name).head_sha = other.head.sha;
  f.args.github.rest.pulls.list = async () => ({data: [f.pull, other]});
  f.args.github.rest.pulls.get = async ({pull_number}) => ({data: pull_number === 7 ? f.pull : other});
  f.args.github.rest.repos.listCommitStatusesForRef = async ({ref}) => {
    assert.equal(f.writes.filter(w => w.status === 'in_progress').length, 2);
    if (ref === f.pull.head.sha) throw Error('first PR unavailable');
    return {data: f.statuses};
  };
  await assert.rejects(admit(f.args), /Admission refresh failed/);
  assert.deepEqual(f.writes.slice(0, 2).map(w => w.head_sha), [f.pull.head.sha, other.head.sha]);
  assert.equal(f.writes[2].conclusion, 'success');
});

test('pagination overflow fails the sweep after reusing one gate per head', async () => {
  const f = fixture(); delete f.args.context.payload.pull_request;
  f.args.github.rest.pulls.list = async () => ({data: Array(100).fill(f.pull)});
  await assert.rejects(admit(f.args), /500/);
  assert.equal(f.writes.length, 1);
});
