import { act, fireEvent, render, screen } from '@testing-library/react'
import { useState } from 'react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { loadRail, saveRail } from '../../src/App.tsx'
import { useDismiss } from '../../src/TopNav.tsx'

function Menu() {
  const [open, setOpen] = useState(false)
  const ref = useDismiss(open, () => setOpen(false))
  return (
    <div>
      <button type="button">outside</button>
      <div className="pop" ref={ref}>
        <button type="button" aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
          trigger
        </button>
        {open && (
          <div className="pop-menu" role="menu">
            <a href="#a">first</a>
            <button type="button">second</button>
            <button type="button" disabled>
              disabled
            </button>
            <button type="button">third</button>
          </div>
        )}
      </div>
    </div>
  )
}

const flush = () => act(() => new Promise((r) => setTimeout(r, 5)))
const key = (k: string) => fireEvent.keyDown(document.activeElement ?? document.body, { key: k })

describe('useDismiss menu behaviour', () => {
  it('focuses the first item on open and moves with arrows, Home and End, skipping disabled items', async () => {
    render(<Menu />)
    fireEvent.click(screen.getByText('trigger'))
    await flush()
    expect(document.activeElement).toBe(screen.getByText('first'))
    key('ArrowDown')
    expect(document.activeElement).toBe(screen.getByText('second'))
    key('ArrowDown')
    expect(document.activeElement).toBe(screen.getByText('third'))
    key('ArrowDown') // wraps
    expect(document.activeElement).toBe(screen.getByText('first'))
    key('ArrowUp') // wraps back
    expect(document.activeElement).toBe(screen.getByText('third'))
    key('Home')
    expect(document.activeElement).toBe(screen.getByText('first'))
    key('End')
    expect(document.activeElement).toBe(screen.getByText('third'))
  })

  it('Escape closes and returns focus to the trigger', async () => {
    render(<Menu />)
    fireEvent.click(screen.getByText('trigger'))
    await flush()
    key('Escape')
    expect(screen.queryByRole('menu')).toBeNull()
    expect(document.activeElement).toBe(screen.getByText('trigger'))
  })

  it('closes on outside mousedown and when focus leaves the menu', async () => {
    render(<Menu />)
    fireEvent.click(screen.getByText('trigger'))
    await flush()
    fireEvent.mouseDown(screen.getByText('outside'))
    expect(screen.queryByRole('menu')).toBeNull()

    fireEvent.click(screen.getByText('trigger'))
    await flush()
    fireEvent.focusOut(screen.getByText('first'), { relatedTarget: screen.getByText('outside') })
    expect(screen.queryByRole('menu')).toBeNull()
  })
})

describe('rail preference storage', () => {
  afterEach(() => {
    vi.restoreAllMocks()
    localStorage.clear()
  })
  it('round-trips', () => {
    saveRail(true)
    expect(loadRail()).toBe(true)
    saveRail(false)
    expect(loadRail()).toBe(false)
  })
  it('defaults to expanded and never throws when storage is blocked', () => {
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new DOMException('blocked', 'SecurityError')
    })
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new DOMException('blocked', 'SecurityError')
    })
    expect(() => saveRail(true)).not.toThrow()
    expect(loadRail()).toBe(false)
  })
})
