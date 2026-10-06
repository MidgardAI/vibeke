import { Component, type ReactNode } from 'react';

/** Renders `fallback` instead of crashing the screen when a subtree throws while rendering. */
export class ErrorBoundary extends Component<{ fallback: ReactNode; children: ReactNode; resetKey?: unknown }, { failed: boolean; key: unknown }> {
  override state = { failed: false, key: this.props.resetKey };

  static getDerivedStateFromError(): Partial<{ failed: boolean }> {
    return { failed: true };
  }

  static getDerivedStateFromProps(props: { resetKey?: unknown }, state: { failed: boolean; key: unknown }) {
    // New content (e.g. a different text) gets a fresh attempt.
    return props.resetKey !== state.key ? { failed: false, key: props.resetKey } : null;
  }

  override componentDidCatch(): void {
    /* the fallback is shown; nothing to report */
  }

  override render(): ReactNode {
    return this.state.failed ? this.props.fallback : this.props.children;
  }
}
