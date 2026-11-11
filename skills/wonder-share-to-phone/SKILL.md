---
name: wonder-share-to-phone
description: Use when the user's message says it came from their phone through Cupola and you are about to give them a localhost or 127.0.0.1 link, a local dev server, or a page to look at. Explains how the page opens on their phone instead.
---

# Showing a page on the user's phone

The user is reading your replies in Cupola on their iPhone or iPad, away from
this computer's screen. On the phone, `localhost`, `127.0.0.1`, `0.0.0.0` and
`file://` point at the phone itself, so those links never open the page. Never
hand them one.

## Use Cupola's Preview

Cupola finds web servers running for the Project and opens them on the phone
through its own connection to this computer, however the phone is connected.
Nothing needs to be published.

1. Start the server from inside one of the Project's folders, listening on
   this computer (`localhost`, `127.0.0.1` or all interfaces), and leave it
   running. A server started elsewhere, or one that has exited, is not shown.
2. Check it answers: `curl -sI http://127.0.0.1:<port>/`.
3. Tell the user to tap Preview, the globe button above the message box. If
   several servers are running, name the one to pick by its page title or
   port. Write the port as plain text ("port 5173"), not as a link.

Preview shows the page at the phone's width and can switch to a desktop
layout, so you do not need a separate mobile build to show it.

## When the user wants an address

Only if the user asks for a link (to open in another browser or on another of
their devices) and this computer has Tailscale, serve the page on their
tailnet:

1. Check Tailscale with `tailscale status`. On a Mac the command may not be on
   `PATH`; use `/Applications/Tailscale.app/Contents/MacOS/Tailscale`. If
   Tailscale is stopped or the device is listed as offline, say so rather
   than sending a link that cannot open.
2. Read this computer's tailnet name from `tailscale status --json`
   (`Self.DNSName`, without the trailing dot).
3. Run `tailscale serve status` and choose an HTTPS port it does not list.
   Never reset or overwrite an existing entry.
4. `tailscale serve --bg --https=<port> http://127.0.0.1:<local port>`
5. Check it answers (`curl -sI https://<name>:<port>`), then give the user
   `https://<name>:<port>`.

Never use `tailscale funnel`: it publishes the page to the whole internet.
When the user is done, or your task ends, remove what you added with
`tailscale serve --https=<port> off` and say that the link is gone.

## Other ways to show it

- A public address (a deployed site, a pull request, a hosted preview) needs
  nothing extra: send it.
- If the page cannot be served, take a screenshot and show that instead (see
  "Showing a result on the phone" below).
- Sharing this computer's own signed-in browser or screen is a different job.
  Ask first.

## Showing a result on the phone

Cupola can open files inside the Project's folders. To show a screenshot or a
generated file, save it inside the Project (somewhere it will not be
committed by accident) and give its path relative to the Project.
