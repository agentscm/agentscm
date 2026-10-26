import { Component, type ErrorInfo, type ReactNode } from 'react'

/** Last line of defence: a render error shows a way back instead of a blank page. */
export class ErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state = { error: null as Error | null }

  static getDerivedStateFromError(error: Error) {
    return { error }
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('AgentSCM Hub page error', error, info.componentStack)
  }

  render() {
    if (!this.state.error) return this.props.children
    return (
      <div className="crash" role="alert">
        <h1 className="page-question">This page hit a problem</h1>
        <p>
          Something on this view could not be shown, often because the address points at something that does not exist. Nothing on the hub was changed.
        </p>
        <p className="muted small">
          <code>{this.state.error.message}</code>
        </p>
        <a
          href="#home"
          className="btn btn-primary"
          onClick={(e) => {
            e.preventDefault()
            // Reset the address first: the hash that caused the error would otherwise reproduce it.
            window.location.hash = 'home'
            this.setState({ error: null })
          }}
        >
          Go to the dashboard
        </a>
      </div>
    )
  }
}
