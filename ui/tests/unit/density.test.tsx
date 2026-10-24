import { fireEvent, render, screen, within } from '@testing-library/react'
import { useEffect, useState } from 'react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { SettingsPage } from '../../src/pages/Settings.tsx'
import { loadDensity, saveDensity, type Density } from '../../src/storage.ts'

afterEach(() => {
  vi.restoreAllMocks()
  localStorage.clear()
  delete document.documentElement.dataset['density']
})

describe('density storage', () => {
  it('defaults to spacious, round-trips, and ignores junk', () => {
    expect(loadDensity()).toBe('spacious')
    saveDensity('compact')
    expect(loadDensity()).toBe('compact')
    localStorage.setItem('agentscm.density', 'tiny')
    expect(loadDensity()).toBe('spacious')
  })
  it('never throws when storage is blocked', () => {
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new DOMException('blocked', 'SecurityError')
    })
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new DOMException('blocked', 'SecurityError')
    })
    expect(() => saveDensity('compact')).not.toThrow()
    expect(loadDensity()).toBe('spacious')
  })
})

// Mirrors App: state, saved on change, applied to <html>.
function Host() {
  const [density, setDensity] = useState<Density>(loadDensity)
  useEffect(() => {
    document.documentElement.dataset['density'] = density
  }, [density])
  return (
    <SettingsPage token="t" whoami={null} pref="system" onPref={() => {}} density={density} onDensity={(d) => { setDensity(d); saveDensity(d) }} railCollapsed={false} onRail={() => {}} onForget={() => {}} />
  )
}

describe('Settings density choice', () => {
  it('switches data-density and remembers it', () => {
    render(<Host />)
    const group = screen.getByRole('radiogroup', { name: 'Density' })
    expect(group.getAttribute('data-tour')).toBe('settings-density')
    expect(document.documentElement.dataset['density']).toBe('spacious')
    fireEvent.click(screen.getByRole('radio', { name: /Compact/ }))
    expect(document.documentElement.dataset['density']).toBe('compact')
    expect(screen.getByRole('radio', { name: /Compact/ }).getAttribute('aria-checked')).toBe('true')
    expect(loadDensity()).toBe('compact')
  })

  it('roves with the arrow keys, Home and End: one tab stop, selection follows focus', () => {
    render(<Host />)
    const radios = () => within(screen.getByRole('radiogroup', { name: 'Density' })).getAllByRole('radio')
    expect(radios().map((r) => r.tabIndex)).toEqual([0, -1])
    const group = screen.getByRole('radiogroup', { name: 'Density' })
    fireEvent.keyDown(radios()[0]!, { key: 'ArrowRight' })
    expect(document.documentElement.dataset['density']).toBe('compact')
    expect(document.activeElement).toBe(radios()[1])
    expect(radios().map((r) => r.tabIndex)).toEqual([-1, 0])
    fireEvent.keyDown(group, { key: 'ArrowDown' })
    expect(document.documentElement.dataset['density']).toBe('spacious')
    expect(document.activeElement).toBe(radios()[0])
    fireEvent.keyDown(group, { key: 'End' })
    expect(document.activeElement).toBe(radios()[1])
    fireEvent.keyDown(group, { key: 'ArrowUp' })
    fireEvent.keyDown(group, { key: 'Home' })
    expect(document.activeElement).toBe(radios()[0])
  })

  it('the Theme cards rove too', () => {
    const onPref = vi.fn()
    render(<SettingsPage token="t" whoami={null} pref="system" onPref={onPref} density="spacious" onDensity={() => {}} railCollapsed={false} onRail={() => {}} onForget={() => {}} />)
    fireEvent.keyDown(screen.getByRole('radio', { name: /System/ }), { key: 'ArrowLeft' })
    expect(onPref).toHaveBeenCalledWith('light')
    fireEvent.keyDown(screen.getByRole('radiogroup', { name: 'Theme' }), { key: 'End' })
    expect(onPref).toHaveBeenLastCalledWith('dark')
  })
})
