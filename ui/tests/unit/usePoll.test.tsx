import { beforeEach, describe, expect, it } from 'vitest'
import { renderHook, waitFor } from '@testing-library/react'
import { clearPollCache, usePoll } from '../../src/usePoll.ts'

describe('usePoll', () => {
  beforeEach(clearPollCache)

  it('shows the last value at once when a page comes back, then refreshes it', async () => {
    let release: (v: string) => void = () => {}
    const load = (): Promise<string> => new Promise((r) => { release = r })
    const first = renderHook(() => usePoll(load, 0, 'page'))
    release('A')
    await waitFor(() => expect(first.result.current).toMatchObject({ kind: 'ready', value: 'A' }))
    first.unmount()
    const again = renderHook(() => usePoll(load, 0, 'page'))
    expect(again.result.current).toMatchObject({ kind: 'ready', value: 'A' })
    release('B')
    await waitFor(() => expect(again.result.current).toMatchObject({ kind: 'ready', value: 'B' }))
    clearPollCache()
    again.unmount()
    expect(renderHook(() => usePoll(load, 0, 'page')).result.current.kind).toBe('loading')
  })

  it('reads as loading when the key changes, until the new key has loaded', async () => {
    let release: (v: string) => void = () => {}
    const load = (): Promise<string> => new Promise((r) => { release = r })
    const { result, rerender } = renderHook(({ k }) => usePoll(load, 0, k), { initialProps: { k: 'a' } })
    release('A')
    await waitFor(() => expect(result.current).toMatchObject({ kind: 'ready', value: 'A' }))
    rerender({ k: 'b' })
    expect(result.current.kind).toBe('loading')
    release('B')
    await waitFor(() => expect(result.current).toMatchObject({ kind: 'ready', value: 'B' }))
  })

  it('keeps the shown value while a refresh nonce re-fetches', async () => {
    let n = 0
    let release: (v: string) => void = () => {}
    const load = (): Promise<string> => new Promise((r) => { n++; release = r })
    const { result, rerender } = renderHook(({ r }) => usePoll(load, 0, 'k', r), { initialProps: { r: 0 } })
    release('A')
    await waitFor(() => expect(result.current).toMatchObject({ kind: 'ready', value: 'A' }))
    rerender({ r: 1 })
    expect(result.current).toMatchObject({ kind: 'ready', value: 'A' })
    await waitFor(() => expect(n).toBe(2))
    release('B')
    await waitFor(() => expect(result.current).toMatchObject({ kind: 'ready', value: 'B' }))
  })
})
