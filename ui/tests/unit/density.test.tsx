import { fireEvent, render, screen } from '@testing-library/react'
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
})
