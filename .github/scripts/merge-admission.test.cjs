const {test} = require('node:test');
const assert = require('node:assert/strict');
const {admit, pages} = require('./merge-admission.cjs');

function fixture() {
  const pull = {number: 7, state: 'open', head: {sha: 'a'.repeat(40)}, base: {ref: 'main'}, title: 'fix: Example'};
  const statuses = [{context: 'agentscm/local-validation', state: 'success', creator: {id: 41898282}}];
  const checks = [{id: 4, name: 'Conventional PR title', status: 'completed', conclusion: 'success', app: {id: 15368}}];
  const writes = [];
  let reads = 0;
  const f = {pull, statuses, checks, writes};
  f.args = {
    context: {repo: {owner: 'synthetic', repo: 'example'}, payload: {repository: {default_branch: 'main'}, pull_request: pull}},
    validateTitle: title => title.startsWith('fix: '),
    github: {rest: {
      pulls: {
        get: async () => ({data: {...pull, head: {sha: ++reads === 2 && f.moved ? 'b'.repeat(40) : pull.head.sha}}}),
        list: async () => ({data: [pull]}),
      },
      repos: {listCommitStatusesForRef: async () => {
        if (f.apiError) throw Error('unavailable');
        return {data: statuses};
      }},
      checks: {
        listForRef: async () => ({data: {check_runs: checks}}),
        create: async value => {writes.push(value); return {data: {id: 90}};},
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

test('prerequisite start invalidates prior green without reading old success', async () => {
  for (const action of ['requested', 'in_progress']) {
    const f = fixture(); delete f.args.context.payload.pull_request;
    f.args.context.payload.workflow_run = {id: 3};
    f.args.context.payload.action = action;
    f.apiError = true;
    await admit(f.args);
    assert.equal(f.writes.length, 1);
    assert.equal(f.writes[0].status, 'in_progress');
  }
});


test('one PR API failure cannot preserve another PR old acceptance', async () => {
  const f = fixture(); delete f.args.context.payload.pull_request;
  const other = {...f.pull, number: 8, head: {sha: 'c'.repeat(40)}};
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

test('observed pages are invalidated before pagination overflow', async () => {
  const f = fixture(); delete f.args.context.payload.pull_request;
  f.args.github.rest.pulls.list = async () => ({data: Array(100).fill(f.pull)});
  await assert.rejects(admit(f.args), /500/);
  assert.equal(f.writes.length, 500);
  assert.ok(f.writes.every(w => w.status === 'in_progress'));
});
