import { afterEach, describe, expect, it, vi } from 'vitest'
import { saveToken } from '../../src/storage.ts'
import routerWire from '../fixtures/workflow-library-router.json'
import {
  acceptSkill, createWorkflow, fetchAttachments, fetchSkillLibrary, fetchWorkflowLibrary, previewSkill, publishWorkflow, simulateWorkflow, updateWorkflow, validateAttachment, WorkflowLibraryError,
  type Attachment, type SkillPreview, type WorkflowDraft,
} from '../../src/workflowLibrary.ts'

const TOKEN = `local:${'a'.repeat(64)}:acme/widgets`
const signal = new AbortController().signal
const response = (body: unknown, status = 200) => new Response(status === 204 ? null : JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
const draft: WorkflowDraft = { id: 'release', name: 'Release prep', summary: 'Prepare release evidence', generation: 3, steps: [], trigger: null, follow_on: null, updated_at: 1_798_000_000, archived: false }

afterEach(() => {
  vi.unstubAllGlobals()
  localStorage.clear()
  sessionStorage.clear()
})

describe('workflow library client', () => {
  it('requires the agentscm open browser grant instead of falling back to sample data', async () => {
    await expect(fetchWorkflowLibrary(signal)).rejects.toMatchObject({ code: 'not_connected', status: 401 })
  })

  it('decodes bounded workflow and skill lists and sends the local grant', async () => {
    saveToken(TOKEN, true)
    const calls: { path: string; auth: string | null }[] = []
    vi.stubGlobal('fetch', async (input: string, init: RequestInit) => {
      calls.push({ path: input, auth: new Headers(init.headers).get('authorization') })
      if (input.endsWith('skill-library')) return response({ version: 1, skills: [{ id: 'skill-1', name: 'Verify', version: 1, generation: 1, digest: 'b'.repeat(64), entrypoint: 'SKILL.md', files: [], provenance: { kind: 'markdown', locator: 'pasted' }, created_at: 1, archived: false }] })
      return response({ version: 1, workflows: [{ draft, versions: [] }] })
    })
    expect((await fetchWorkflowLibrary(signal))[0]!.draft.name).toBe('Release prep')
    expect((await fetchSkillLibrary(signal))[0]).toMatchObject({ name: 'Verify', provenance: { kind: 'markdown' } })
    expect(calls).toEqual([
      { path: '/v1/local/workflow-library', auth: `Bearer ${TOKEN}` },
      { path: '/v1/local/skill-library', auth: `Bearer ${TOKEN}` },
    ])
  })

  it('uses idempotency on create, generation CAS on save, and exact tagged steps', async () => {
    saveToken(TOKEN, true)
    const bodies: Record<string, unknown>[] = []
    vi.stubGlobal('fetch', async (_input: string, init: RequestInit) => {
      bodies.push(JSON.parse(String(init.body)) as Record<string, unknown>)
      return response({ draft: { ...draft, generation: bodies.length + 3, steps: bodies.at(-1)?.['steps'] ?? [] } })
    })
    const created = await createWorkflow('Release prep', 'Prepare release evidence', signal)
    expect(bodies[0]).toMatchObject({ name: 'Release prep', summary: 'Prepare release evidence', idempotency_key: expect.stringMatching(/^workflow-/) })
    await updateWorkflow({ ...created, steps: [{ id: 'build-1', label: 'Build', needs: [], required: true, kind: 'build', skill_version_id: 'skill-1', runner: 'runners/build.json' }] }, signal)
    expect(bodies[1]).toEqual({ expected_generation: 4, name: 'Release prep', summary: 'Prepare release evidence', steps: [{ id: 'build-1', label: 'Build', needs: [], required: true, kind: 'build', skill_version_id: 'skill-1', runner: 'runners/build.json' }], trigger: null, follow_on: null })
  })

  it('previews npx as a descriptor and preserves zero-side-effect simulation semantics', async () => {
    saveToken(TOKEN, true)
    const calls: { path: string; body: Record<string, unknown> }[] = []
    vi.stubGlobal('fetch', async (input: string, init: RequestInit) => {
      const body = JSON.parse(String(init.body)) as Record<string, unknown>
      calls.push({ path: input, body })
      if (input.endsWith('preview')) return response({ preview_id: 'p1', digest: 'c'.repeat(64), expires_at: 2, skill: { name: 'Check', entrypoint: 'SKILL.md', provenance: { kind: 'npx_descriptor', locator: 'owner/repo', commit: 'd'.repeat(40), path: 'skills/check/SKILL.md', command: 'npx skills add owner/repo --skill check' } }, files: [], warnings: [], blocked: false })
      return response({ outcome: 'passed_with_warnings', steps: [{ id: 'deploy-1', status: 'would_run', requires: ['operator_profile'], detail: 'Would dispatch a retained action.' }], diagnostics: [{ severity: 'warning', message: 'Runtime is not configured.' }], side_effects: 'none', admission: 'not_assessed', deployment: 'not_attempted' })
    })
    await previewSkill({ kind: 'npx_descriptor', command: 'npx skills add owner/repo --skill check', commit: 'd'.repeat(40), path: 'skills/check/SKILL.md', resources: [] }, signal)
    const sim = await simulateWorkflow('wf-v1', 'acme/widgets', undefined, signal)
    expect(calls[0]!.body).toEqual({ source: { kind: 'npx_descriptor', command: 'npx skills add owner/repo --skill check', commit: 'd'.repeat(40), path: 'skills/check/SKILL.md', resources: [] } })
    expect(sim).toMatchObject({ outcome: 'passed_with_warnings', side_effects: 'none', admission: 'not_assessed', deployment: 'not_attempted' })
  })

  it('sends the exact GitHub import discriminator and immutable commit', async () => {
    saveToken(TOKEN, true)
    let sent: Record<string, unknown> = {}
    vi.stubGlobal('fetch', async (_input: string, init: RequestInit) => {
      sent = JSON.parse(String(init.body)) as Record<string, unknown>
      return response({ preview_id: 'p1', digest: 'c'.repeat(64), expires_at: 2, skill: { name: 'Check', entrypoint: 'SKILL.md', provenance: { kind: 'github', locator: 'owner/repo', commit: 'd'.repeat(40), path: 'skills/check/SKILL.md' } }, files: [], warnings: [], blocked: false })
    })
    await previewSkill({ kind: 'github', repository: 'owner/repo', commit: 'd'.repeat(40), path: 'skills/check/SKILL.md', resources: [] }, signal)
    expect(sent).toEqual({ source: { kind: 'github', repository: 'owner/repo', commit: 'd'.repeat(40), path: 'skills/check/SKILL.md', resources: [] } })
  })

  it('surfaces typed CAS errors with recovery copy', async () => {
    saveToken(TOKEN, true)
    vi.stubGlobal('fetch', async () => response({ error: 'conflict: workflow generation conflict' }, 409))
    await expect(updateWorkflow(draft, signal)).rejects.toEqual(expect.objectContaining<Partial<WorkflowLibraryError>>({ status: 409, code: 'http_409', message: 'conflict: workflow generation conflict' }))
  })

  it('decodes bare accept and attachment responses from the Rust handlers', async () => {
    saveToken(TOKEN, true)
    const accepted = { id: 'skl_1', name: 'Release helper', version: 1, generation: 1, digest: 'c'.repeat(64), entrypoint: 'SKILL.md', files: [], provenance: { kind: 'markdown', locator: 'pasted' }, created_at: 1, archived: false }
    const attached = { id: 'release', repository: 'acme/widgets', workflow_version_id: 'wfv_1', generation: 2, state: 'export_required', params: {}, source_binding: { source_revision: 'd'.repeat(40) }, pipeline_rule: null, diagnostics: [] }
    vi.stubGlobal('fetch', async (input: string) => input.endsWith('accept') ? response(accepted) : response([attached]))
    const preview: SkillPreview = { preview_id: 'p1', digest: accepted.digest, expires_at: 2, skill: { name: accepted.name, entrypoint: accepted.entrypoint, provenance: { kind: 'markdown', locator: 'pasted' } }, files: [], warnings: [], blocked: false }
    await expect(acceptSkill(preview, signal)).resolves.toMatchObject({ id: 'skl_1', name: 'Release helper' })
    await expect(fetchAttachments('acme/widgets', signal)).resolves.toEqual([expect.objectContaining({ id: 'release', source_binding: { source_revision: 'd'.repeat(40) } })])
  })

  it('retains the exact operator setup returned by attachment validation', async () => {
    saveToken(TOKEN, true)
    let sent: Record<string, unknown> = {}
    vi.stubGlobal('fetch', async (_input: string, init: RequestInit) => {
      sent = JSON.parse(String(init.body)) as Record<string, unknown>
      return response({
        attachment: { id: 'release', repository: 'acme/widgets', workflow_version_id: 'release-v1', generation: 2, state: 'export_required', diagnostics: [] },
        state: 'export_required',
        setup: { runnable: false, files: [{ path: '.agentscm/release.json', content: '{"exact":true}' }], pipeline_recipe: { trigger: 'confirmed_merge' }, pipeline_install_template: { command: 'agentscm pipeline install' }, library_follow_on: null, unmet_bindings: ['operator_profile'], activation: 'operator_local_cli' },
        next_actions: ['Commit the exported package.'],
      })
    })
    const value: Attachment = { id: 'release', repository: 'acme/widgets', workflow_version_id: 'release-v1', generation: 1, state: 'draft', diagnostics: [] }
    const result = await validateAttachment('acme/widgets', value, '1'.repeat(40), signal)
    expect(sent).toEqual({ expected_generation: 1, source_revision: '1'.repeat(40) })
    expect(result).toMatchObject({ state: 'export_required', setup: { runnable: false, activation: 'operator_local_cli', unmet_bindings: ['operator_profile'], files: [{ path: '.agentscm/release.json', content: '{"exact":true}' }] }, next_actions: ['Commit the exported package.'] })
  })

  it('decodes the sanitized bodies captured from a granted Rust router round trip', async () => {
    saveToken(TOKEN, true)
    const replies: { body: unknown; status?: number }[] = [
      { body: routerWire['preview'] },
      { body: routerWire['accepted_skill'] },
      { body: routerWire['skills'] },
      { body: routerWire['workflows'] },
      { body: routerWire['attachments_reloaded'] },
      { body: routerWire['simulation'] },
      { body: routerWire['attachment_validated'] },
      { body: routerWire['publication_rejected'], status: 400 },
      { body: routerWire['conflict'], status: 409 },
    ]
    vi.stubGlobal('fetch', async () => {
      const next = replies.shift()
      if (!next) throw new Error('Unexpected client request')
      return response(next.body, next.status)
    })
    const preview = await previewSkill({ kind: 'markdown', name: 'Release helper', markdown: '# Release helper', resources: [] }, signal)
    expect(preview).toMatchObject({ preview_id: 'imp_fixture', blocked: false, skill: { name: 'Release helper', provenance: { kind: 'markdown' } } })
    await expect(acceptSkill(preview, signal)).resolves.toMatchObject({ id: 'skl_fixture', name: 'Release helper' })
    await expect(fetchSkillLibrary(signal)).resolves.toEqual([expect.objectContaining({ id: 'skl_fixture' })])
    const workflows = await fetchWorkflowLibrary(signal)
    expect(workflows.some((item) => item.draft.id === 'wfl_parent' && item.versions.some((item) => item.id === 'wfv_parent'))).toBe(true)
    const attachments = await fetchAttachments('acme/widgets', signal)
    expect(attachments).toEqual([expect.objectContaining({ id: 'major-feature', workflow_version_id: 'wfv_parent' })])
    await expect(simulateWorkflow('wfv_parent', 'acme/widgets', '1'.repeat(40), signal)).resolves.toMatchObject({ outcome: 'passed', side_effects: 'none' })
    await expect(validateAttachment('acme/widgets', attachments[0]!, '1'.repeat(40), signal)).resolves.toMatchObject({ state: 'export_required', setup: { activation: 'operator_local_cli' } })
    await expect(publishWorkflow(draft, signal)).rejects.toEqual(expect.objectContaining({ status: 400, code: 'invalid_workflow', message: 'workflow has blocking diagnostics', diagnostics: [expect.objectContaining({ severity: 'error' })] }))
    await expect(updateWorkflow(draft, signal)).rejects.toEqual(expect.objectContaining({ status: 409, message: 'conflict: workflow generation conflict' }))
    expect(replies).toHaveLength(0)
  })
})
