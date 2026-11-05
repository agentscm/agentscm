// Trusted default-branch orchestration only. Never reads or executes PR contents.
const NAME = 'Merge batch gate';
const ACTIONS_APP = 15368;
const ACTIONS_BOT = 41898282;

async function pages(call, key, visit = async () => {}) {
  const values = [];
  for (let page = 1; page <= 5; page++) {
    const response = await call({per_page: 100, page});
    const batch = key ? response.data[key] : response.data;
    await visit(batch);
    values.push(...batch);
    if (batch.length < 100) return values;
  }
  throw new Error('Admission API coverage exceeds 500 records');
}

async function admit({github, context, validateTitle}) {
  const repo = context.repo;
  const pending = [];
  const errors = [];
  // Invalidate each observed page before fetching another. No prerequisite read can
  // abort the sweep while another known head still carries an older acceptance.
  async function invalidate(candidates) {
    for (const candidate of candidates) {
      try {
        const create = head => github.rest.checks.create({
          ...repo, name: NAME, head_sha: head, status: 'in_progress',
          output: {title: 'PR admission pending', summary: 'Checking current local validation and title; this does not qualify a merge batch.'},
        });
        let {data: check} = await create(candidate.head.sha);
        const {data: pull} = await github.rest.pulls.get({...repo, pull_number: candidate.number});
        if (pull.state !== 'open' || pull.base.ref !== context.payload.repository.default_branch) continue;
        if (pull.head.sha !== candidate.head.sha) ({data: check} = await create(pull.head.sha));
        pending.push({pull, check});
      } catch (error) { errors.push(error); }
    }
  }
  if (context.payload.pull_request) {
    await invalidate([context.payload.pull_request]);
  } else {
    // Overflow or a later page error leaves all already-observed pages pending.
    await pages(args => github.rest.pulls.list({...repo, state: 'open', base: context.payload.repository.default_branch, ...args}), undefined, invalidate);
  }
  for (const {pull, check} of pending) {
    const head = pull.head.sha;
    try {
    // Prerequisite workflows invalidate prior acceptance as soon as they start. Their
    // completion event will recompute; an out-of-order start can only leave pending.
    if (context.payload.workflow_run && context.payload.action !== 'completed') continue;
    const statuses = await pages(args => github.rest.repos.listCommitStatuses({...repo, ref: head, ...args}));
    const checks = await pages(args => github.rest.checks.listForRef({...repo, ref: head, filter: 'latest', ...args}), 'check_runs');
    // GitHub returns newest statuses first. A newer pending/failing status must defeat old success.
    const local = statuses.find(s => s.context === 'agentscm/local-validation');
    const title = checks.filter(c => c.name === 'Conventional PR title').sort((a, b) => b.id - a.id)[0];
    const trustedLocal = local?.creator?.id === ACTIONS_BOT;
    const trustedTitle = title?.app?.id === ACTIONS_APP;
    const currentTitleValid = validateTitle(pull.title);
    let conclusion;
    if (!currentTitleValid || (trustedLocal && ['failure', 'error'].includes(local.state)) ||
        (trustedTitle && title.status === 'completed' && title.conclusion !== 'success')) {
      conclusion = 'failure';
    } else if (trustedLocal && local.state === 'success' && trustedTitle && title.status === 'completed' && title.conclusion === 'success') {
      conclusion = 'success';
    }
    // Re-read after the API sweep. Never certify an obsolete head/base/title snapshot.
    const {data: current} = await github.rest.pulls.get({...repo, pull_number: pull.number});
    if (current.head.sha !== head || current.base.ref !== pull.base.ref || current.title !== pull.title || current.state !== 'open') continue;
    await github.rest.checks.update({
      ...repo, check_run_id: check.id,
      status: conclusion ? 'completed' : 'in_progress',
      ...(conclusion ? {conclusion} : {}),
      output: {
        title: conclusion === 'success' ? 'PR admitted; batch tests still required' : 'PR admission blocked',
        summary: `Exact head ${head}. Local validation and current Conventional PR title must pass from GitHub Actions. Missing, untrusted or pending results remain pending. Batch qualification is a separate merge-group run.`,
      },
    });
    } catch (error) { errors.push(error); }
  }
  if (errors.length) throw new AggregateError(errors, "Admission refresh failed; affected heads remain pending");
}
module.exports = {admit, pages};
