import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { Combobox, type ComboOption } from '../../src/Combobox.tsx'

const opt = (value: string, text = value): ComboOption => ({ value, text, label: text, secondary: `id-${value}` })
const OPTIONS = [opt('a', 'Alpha branch'), opt('b', 'Beta branch'), opt('c', 'Gamma')]
const input = () => screen.getByRole('combobox', { name: 'Pick' }) as HTMLInputElement
const idOnly = (t: string) => (/^id_[0-9]+$/.test(t) ? t : null)

describe('Combobox (client filter)', () => {
  it('exposes the ARIA 1.2 combobox contract', () => {
    render(<Combobox id="p" label="Pick" options={OPTIONS} onSelect={() => {}} data-tour="x-picker" />)
    expect(input().getAttribute('aria-expanded')).toBe('false')
    expect(input().getAttribute('aria-autocomplete')).toBe('list')
    expect(input().getAttribute('aria-controls')).toBe('p-listbox')
    expect(document.querySelector('[data-tour="x-picker"]')).toBeTruthy()
    fireEvent.focus(input())
    expect(input().getAttribute('aria-expanded')).toBe('true')
    const list = screen.getByRole('listbox')
    expect(list.id).toBe('p-listbox')
    expect(screen.getAllByRole('option')).toHaveLength(3)
    expect(screen.getAllByRole('option').every((o) => o.getAttribute('aria-selected') === 'false')).toBe(true)
    // The ID is secondary text, the label is primary.
    expect(screen.getAllByRole('option')[0]!.textContent).toContain('Alpha branch')
    expect(screen.getAllByRole('option')[0]!.querySelector('code')?.textContent).toBe('id-a')
  })

  it('filters as you type and says when nothing matches', () => {
    render(<Combobox id="p" label="Pick" options={OPTIONS} onSelect={() => {}} />)
    fireEvent.focus(input())
    fireEvent.change(input(), { target: { value: 'branch' } })
    expect(screen.getAllByRole('option')).toHaveLength(2)
    fireEvent.change(input(), { target: { value: 'zzz' } })
    expect(screen.queryAllByRole('option')).toHaveLength(0)
    expect(screen.getByText('No matches')).toBeTruthy()
  })

  it('moves with arrows, Home and End, wraps, selects with Enter and closes with Escape and Tab', () => {
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" options={OPTIONS} onSelect={onSelect} />)
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    expect(input().getAttribute('aria-expanded')).toBe('true')
    const active = () => screen.getAllByRole('option').findIndex((o) => o.getAttribute('aria-selected') === 'true')
    expect(active()).toBe(0)
    expect(input().getAttribute('aria-activedescendant')).toBe(screen.getAllByRole('option')[0]!.id)
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    expect(active()).toBe(1)
    fireEvent.keyDown(input(), { key: 'End' })
    expect(active()).toBe(2)
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    expect(active()).toBe(0)
    fireEvent.keyDown(input(), { key: 'ArrowUp' })
    expect(active()).toBe(2)
    fireEvent.keyDown(input(), { key: 'Home' })
    expect(active()).toBe(0)
    fireEvent.keyDown(input(), { key: 'Escape' })
    expect(input().getAttribute('aria-expanded')).toBe('false')
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    fireEvent.keyDown(input(), { key: 'Enter' })
    expect(onSelect).toHaveBeenCalledWith('b', expect.objectContaining({ value: 'b' }))
    expect(input().getAttribute('aria-expanded')).toBe('false')
    fireEvent.focus(input())
    fireEvent.blur(input())
    expect(input().getAttribute('aria-expanded')).toBe('false')
  })

  it('selects with a click', () => {
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" options={OPTIONS} onSelect={onSelect} />)
    fireEvent.focus(input())
    fireEvent.click(screen.getByRole('option', { name: /Gamma/ }))
    expect(onSelect).toHaveBeenCalledWith('c', expect.anything())
  })

  it('offers "Open <id>" first for a pasted ID and opens it with Enter', () => {
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" options={OPTIONS} parseId={idOnly} onSelect={onSelect} />)
    fireEvent.focus(input())
    fireEvent.change(input(), { target: { value: ' id_42 ' } })
    expect(screen.getAllByRole('option')[0]!.textContent).toBe('Open id_42')
    fireEvent.keyDown(input(), { key: 'Enter' })
    expect(onSelect).toHaveBeenCalledWith('id_42', expect.anything())
  })

  it('does not offer to open text that is not an ID', () => {
    render(<Combobox id="p" label="Pick" options={OPTIONS} parseId={idOnly} onSelect={() => {}} />)
    fireEvent.focus(input())
    fireEvent.change(input(), { target: { value: '../etc' } })
    expect(screen.queryByText(/^Open/)).toBeNull()
  })

  it('keeps the picked text when asked to', () => {
    render(<Combobox id="p" label="Pick" options={OPTIONS} keepText onSelect={() => {}} />)
    fireEvent.focus(input())
    fireEvent.click(screen.getByRole('option', { name: /Beta/ }))
    expect(input().value).toBe('Beta branch')
    // Reopening shows everything again until the person types.
    fireEvent.focus(input())
    expect(screen.getAllByRole('option')).toHaveLength(3)
  })
})

describe('Combobox (server search)', () => {
  it('debounces typing, aborts the previous request and shows only the latest answer', async () => {
    const calls: { q: string; signal: AbortSignal }[] = []
    const search = (q: string, signal: AbortSignal) => {
      calls.push({ q, signal })
      return new Promise<{ options: ComboOption[] }>((resolve, reject) => {
        signal.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')))
        if (q === 'ab') window.setTimeout(() => resolve({ options: [opt('late', 'Stale result')] }), 600)
        else resolve({ options: [opt('x', `Result for ${q || 'nothing'}`)] })
      })
    }
    render(<Combobox id="p" label="Pick" search={search} onSelect={() => {}} />)
    fireEvent.focus(input())
    // The empty query loads straight away.
    await screen.findByRole('option', { name: /Result for nothing/ })
    expect(calls.map((c) => c.q)).toEqual([''])
    fireEvent.change(input(), { target: { value: 'a' } })
    fireEvent.change(input(), { target: { value: 'ab' } })
    expect(screen.getByRole('status').textContent).toContain('Searching')
    await waitFor(() => expect(calls.map((c) => c.q)).toEqual(['', 'ab']))
    fireEvent.change(input(), { target: { value: 'abc' } })
    await waitFor(() => expect(calls.map((c) => c.q)).toEqual(['', 'ab', 'abc']))
    expect(calls[1]!.signal.aborted).toBe(true)
    await screen.findByRole('option', { name: /Result for abc/ })
    await new Promise((r) => setTimeout(r, 700))
    expect(screen.queryByText('Stale result')).toBeNull()
  })

  it('shows no results, then an error with a working retry', async () => {
    let fail = true
    const search = vi.fn(async (q: string) => {
      if (fail) throw new Error('boom')
      return { options: q === 'none' ? [] : [opt('ok', 'Recovered')] }
    })
    render(<Combobox id="p" label="Pick" search={search} onSelect={() => {}} />)
    fireEvent.focus(input())
    expect((await screen.findByRole('alert')).textContent).toContain('Could not search')
    fail = false
    fireEvent.click(screen.getByRole('button', { name: 'Try again' }))
    await screen.findByRole('option', { name: /Recovered/ })
    fireEvent.change(input(), { target: { value: 'none' } })
    await screen.findByText('No matches')
  })

  it('opens a pasted ID without waiting for the server', async () => {
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" search={async () => ({ options: [] })} parseId={idOnly} onSelect={onSelect} />)
    fireEvent.focus(input())
    fireEvent.change(input(), { target: { value: 'id_7' } })
    fireEvent.keyDown(input(), { key: 'Enter' })
    expect(onSelect).toHaveBeenCalledWith('id_7', expect.anything())
  })
})
