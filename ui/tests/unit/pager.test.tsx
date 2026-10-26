import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'
import { sectionFromHash } from '../../src/App.tsx'
import { crumbsFor } from '../../src/TopNav.tsx'
import { SECTIONS } from '../../src/App.tsx'
import { hashPath, hashQuery, Pager, useHashCursor } from '../../src/Pager.tsx'
import { landedIdFromHash } from '../../src/pages/Landed.tsx'
import { timelineRoute } from '../../src/pages/Timeline.tsx'
import { workTabFromHash } from '../../src/pages/Work.tsx'
import { routeFromHash } from '../../src/pages/Workflows.tsx'
import { changeRecordIdFromHash } from '../../src/pages/ChangeRecord.tsx'
import { workspaceRoute } from '../../src/pages/Workspace.tsx'
import { workIdFromHash } from '../../src/standup.ts'

const PUB = `pub_${'a'.repeat(32)}`
const WORK = `wrk_${'b'.repeat(32)}`
const tick = () => act(async () => { await new Promise((r) => setTimeout(r, 20)) })

function Probe() {
  const p = useHashCursor()
  return (
    <div>
      <output aria-label="cursor">{p.cursor ?? 'first'}</output>
      <Pager onNewer={p.canNewer ? p.newer : undefined} onOlder={() => p.older(`c${(p.cursor ?? 'c0').slice(1) === '0' ? 1 : Number((p.cursor ?? 'c0').slice(1)) + 1}`)} />
    </div>
  )
}
const cursor = () => screen.getByLabelText('cursor').textContent

afterEach(() => window.history.replaceState(null, '', '#'))

describe('route parsers ignore a ?query suffix', () => {
  it('splits the hash', () => {
    expect(hashPath('#landed?before=x')).toBe('#landed')
    expect(hashQuery('#landed?before=x&a=1').get('before')).toBe('x')
    expect(hashQuery('#landed').get('before')).toBeNull()
  })
  it('keeps every section parser working with a cursor', () => {
    expect(sectionFromHash('#landed?before=pub_x')).toBe('landed')
    expect(sectionFromHash('#workflows?before=4985')).toBe('workflows')
    expect(sectionFromHash('#timeline?before=3')).toBe('timeline')
    expect(sectionFromHash('#workspace?runs=r1')).toBe('workspace')
    expect(sectionFromHash('#?before=1')).toBe('home')
    expect(landedIdFromHash('#landed?before=x')).toBeNull()
    expect(landedIdFromHash(`#landed/${PUB}?before=x`)).toBe(PUB)
    expect(routeFromHash('#workflows?before=4985')).toEqual({ kind: 'list' })
    expect(routeFromHash('#workflows/runs/run-1/steps/s1?x=1')).toEqual({ kind: 'run', runId: 'run-1', stepId: 's1' })
    expect(timelineRoute('#timeline?before=3')).toEqual({ kind: 'list' })
    expect(timelineRoute('#timeline/entry/4?x=1')).toEqual({ kind: 'entry', id: 4 })
    expect(workTabFromHash('#work/history?x=1')).toBe('history')
    expect(workIdFromHash(`#work/${WORK}?x=1`)).toBe(WORK)
    expect(changeRecordIdFromHash(`#change-record/${WORK}?x=1`)).toBe(WORK)
    expect(workspaceRoute('#workspace/members?x=1')).toEqual({ tab: 'members' })
  })
  it('breadcrumbs ignore the cursor', () => {
    const section = (k: string) => SECTIONS.find((s) => s.key === k)!
    expect(crumbsFor('#landed?before=pub_x', section('landed'), null)).toHaveLength(1)
    expect(crumbsFor('#workflows?before=4985', section('workflows'), null)).toHaveLength(1)
    expect(crumbsFor('#timeline?before=3', section('timeline'), null)).toHaveLength(1)
  })
})

describe('useHashCursor and Pager', () => {
  it('reads a cursor from a link and Newer from a link returns to the first page', () => {
    window.history.replaceState(null, '', '#landed?before=pub_z')
    render(<Probe />)
    expect(cursor()).toBe('pub_z')
    fireEvent.click(screen.getByRole('button', { name: /Newer/ }))
    expect(cursor()).toBe('first')
    expect(window.location.hash).toBe('#landed')
  })

  it('Older writes the cursor to the hash and Newer steps back through the stack', () => {
    window.history.replaceState(null, '', '#landed')
    render(<Probe />)
    fireEvent.click(screen.getByRole('button', { name: /Older/ }))
    expect(window.location.hash).toBe('#landed?before=c1')
    fireEvent.click(screen.getByRole('button', { name: /Older/ }))
    expect(window.location.hash).toBe('#landed?before=c2')
    fireEvent.click(screen.getByRole('button', { name: /Newer/ }))
    expect(window.location.hash).toBe('#landed?before=c1')
    expect(cursor()).toBe('c1')
    fireEvent.click(screen.getByRole('button', { name: /Newer/ }))
    expect(window.location.hash).toBe('#landed')
    expect(cursor()).toBe('first')
  })

  it('the browser Back button returns to the previous page', async () => {
    window.history.replaceState(null, '', '#landed')
    render(<Probe />)
    fireEvent.click(screen.getByRole('button', { name: /Older/ }))
    expect(cursor()).toBe('c1')
    act(() => window.history.back())
    await tick()
    expect(cursor()).toBe('first')
    expect(window.location.hash).toBe('#landed')
  })

  it('a click refocuses only its own Pager when its buttons vanish', () => {
    const Two = ({ gone }: { gone: boolean }) => (
      <>
        <Pager onOlder={() => {}} />
        <Pager onOlder={gone ? undefined : () => {}} complete={gone} />
      </>
    )
    const { rerender } = render(<Two gone={false} />)
    const navs = screen.getAllByRole('navigation', { name: 'Pages' })
    fireEvent.click(screen.getAllByRole('button', { name: /Older/ })[1]!)
    ;(document.activeElement as HTMLElement).blur()
    rerender(<Two gone />)
    expect(document.activeElement).toBe(navs[1])
  })

  it('Pager shows the end state only when there is nothing older', () => {
    const { rerender } = render(<Pager onNewer={() => {}} complete />)
    expect(screen.getByText('That’s everything.')).toBeTruthy()
    expect(screen.getByRole('navigation', { name: 'Pages' }).getAttribute('data-tour')).toBe('pager')
    rerender(<Pager onNewer={() => {}} onOlder={() => {}} complete />)
    expect(screen.queryByText('That’s everything.')).toBeNull()
    rerender(<Pager />)
    expect(screen.queryByRole('navigation')).toBeNull()
  })
})
