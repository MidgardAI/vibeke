# Previews and browsers

A **preview** is a dev server's port made available locally: remote ports are forwarded over the machine link and served through a proxy with origin isolation, bound to loopback. `vibeke preview ...` manages them.

The browser pane renders a page inside the terminal using an isolated Chromium profile; the Chrome DevTools Protocol endpoint is never exposed. Agents get `browser.*` calls for their own sessions. Scripted evaluation is off unless explicitly granted. Screenshots of previews, local or remote, are first-class.
