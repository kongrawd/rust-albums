# HTTPS, sessions and sign-out

## HTTPS only

- There is no plain-HTTP port, so nothing is ever served unencrypted. TLS 1.3 is the minimum by default (`tls.min_version`).
- Browsers are told to use HTTPS only for this host (HSTS). They only honour that for a host name with a trusted certificate, so with the default self-signed certificate on `localhost` or an IP it has no effect; there, the missing HTTP port is what enforces HTTPS.
- Pages run no scripts and can't be framed by other sites, and files open in a sandbox.

## Signing in

Plain HTTP Basic auth can't sign out, because browsers cache the credentials and resend them. So browsers sign in once on the sign-in page and then carry a session cookie.

- One account, from `AUTH_USER` and `AUTH_PASS`.
- **Wrong passwords** slow down only the device that sent them, so they can't lock out the real user. Behind a reverse proxy, every client shares the proxy's address.
- Signing in ends any session the browser already had.
- A session ends after the idle timeout or the maximum lifetime (`[session]` in the config), and every restart signs everyone out ([why](architecture.md#single-replica)).

## Signing out

The **Sign out** button ends the session on the server, so any copy of the cookie stops working, expires the cookie and asks the browser to clear what it cached for the site. Going Back afterwards shows the sign-in page.

## Scripts and tools

`curl -u user:pass` (add `-k` for a self-signed certificate), wget and other tools can send Basic credentials on every request; without them they get a standard Basic challenge. Browsers never use Basic credentials, so a browser that once answered a password prompt can't stay signed in after signing out.

## Direct links

Every page and file URL needs a live session (or Basic credentials), including direct links to photos and videos.

- **Signed out:** a direct link takes you to sign in, then back to the link. Only links to this site are followed.
- **Other sites can't use your session,** even while you're signed in: they can't embed your photos, frame the pages or post forms. A cached photo is only reused after the server confirms the session again.
- **Trade-off:** a link from another site (e.g. an email) arrives without the session cookie, so you sign in again and are then taken to that link.

## Routes

All routes require a session or Basic credentials, except `/login`, `/logout` and `/healthz`.

| Route | |
|---|---|
| `GET /login`, `POST /login` | Sign-in page; `?next=` returns to a direct link |
| `POST /logout` | Sign out |
| `GET /` | Album list |
| `GET /albums/{album_id}` | One album's items |
| `GET /media/{item_id}` | The original file, with Range support (video seeking) |
| `GET /healthz` | 200 `ok`, or 503 with a generic reason while storage is unavailable |
