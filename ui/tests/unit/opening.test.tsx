// @vitest-environment jsdom
import { render, screen, within } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { Opening } from '../../src/pages/Opening.tsx'

describe('Opening roll call', () => {
  it('checks the sign-in before the session is known', () => {
    render(<Opening />)
    expect(screen.getByRole('heading', { level: 1 }).textContent).toBe('See what every agent is working on.')
    expect(screen.getByText('Checking your sign-in').closest('li')?.getAttribute('aria-current')).toBe('step')
  })

  it('shows the login, then the real repository count', () => {
    const { rerender } = render(<Opening login="maria" />)
    expect(screen.getByRole('heading', { level: 1 }).textContent).toBe('Welcome back, maria')
    expect(screen.getByText(/Signed in as maria/)).toBeTruthy()
    expect(screen.getByText('Finding your repositories').closest('li')?.getAttribute('aria-current')).toBe('step')
    rerender(<Opening login="maria" repos={3} />)
    expect(screen.getByText(/Found 3 repositories/)).toBeTruthy()
    expect(screen.getByText('Opening your work').closest('li')?.getAttribute('aria-current')).toBe('step')
    expect(within(screen.getByRole('status', { name: 'Opening the Hub' })).getAllByRole('listitem')).toHaveLength(3)
  })

  it('puts the trouble on the current step with its buttons', () => {
    const onRetry = vi.fn()
    render(<Opening login="maria" repos={1} trouble={{ title: 'Hub connection unavailable', message: 'Try again.', onRetry, retryLabel: 'Retry Hub connection', onSignOut: vi.fn(), signOutLabel: 'Sign out of Hub pages' }} />)
    expect(screen.getByText(/Found 1 repository$/)).toBeTruthy()
    expect(screen.getByText('Opening your work').closest('li')?.getAttribute('data-state')).toBe('bad')
    screen.getByRole('button', { name: 'Retry Hub connection' }).click()
    expect(onRetry).toHaveBeenCalled()
    expect(screen.getByRole('button', { name: 'Sign out of Hub pages' })).toBeTruthy()
  })
})
