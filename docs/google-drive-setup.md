# Google Drive backup: maintainer setup

OmpDeck's "Google Drive backup" card signs in with Google's installed-app OAuth flow and stores
backups in the Drive **app data folder** (`appDataFolder`), a hidden folder only this OAuth client
can read. The OAuth client id and secret are baked into release builds at compile time from the
`OMP_DECK_GOOGLE_CLIENT_ID` / `OMP_DECK_GOOGLE_CLIENT_SECRET` environment variables. A build without
them shows the card as "Not available in this build".

Google treats the secret of a **Desktop app** client as non-confidential: it ships inside every
installed app, and PKCE protects the flow. It still belongs in GitHub secrets so it stays out of
the repository.

## 1. Google Cloud project

1. Open <https://console.cloud.google.com/> and create a project (for example `OmpDeck`).
2. **APIs & Services → Library**: search for **Google Drive API** and click **Enable**.

## 2. OAuth consent screen

**APIs & Services → OAuth consent screen** (called "Google Auth Platform" in newer consoles):

1. **User type: External**.
2. **Branding**: app name `OmpDeck`, user support email, developer contact email, application home
   page `https://github.com/lavinhoque33/omp-deck`, privacy policy
   `https://github.com/lavinhoque33/omp-deck/blob/main/PRIVACY.md`, and authorized domain
   `github.com`. "Publish app" stays disabled until branding is complete. A logo is optional;
   adding one triggers brand verification.
3. **Scopes / Data access → Add or remove scopes**, then add:
   - `https://www.googleapis.com/auth/drive.appdata` (non-sensitive: app folder only)
   - `openid`
   - `.../auth/userinfo.email` (`email`), so the card can show who is signed in
4. **Publish the app: Publishing status → "In production"**. This step is required. While the app
   is in *Testing*, Google expires refresh tokens after **7 days**, and only listed test users can
   sign in. Every scope above is non-sensitive, so production needs no Google verification.
   Users may still see an "unverified app" notice until the brand is verified. That is optional.

## 3. OAuth client

**APIs & Services → Credentials → Create credentials → OAuth client ID**:

- **Application type: Desktop app**, name `OmpDeck desktop`.
- Copy the **Client ID** (`….apps.googleusercontent.com`) and the **Client secret**.

Desktop clients accept any loopback redirect `http://127.0.0.1:<port>`, so no redirect URI needs to
be registered. OmpDeck picks a free port for each sign-in.

## 4. GitHub secrets (release builds)

`.github/workflows/release.yml` passes both secrets to `cargo build --release`:

```sh
gh secret set OMP_DECK_GOOGLE_CLIENT_ID --repo lavinhoque33/omp-deck --body '1234-abc.apps.googleusercontent.com'
gh secret set OMP_DECK_GOOGLE_CLIENT_SECRET --repo lavinhoque33/omp-deck --body 'GOCSPX-…'
```

To verify a release binary, open Settings → OmpDeck → Google Drive backup. It should show
**Sign in with Google**, not "Not available in this build".

## 5. Local development

Set the variables at build time to bake them in, or at runtime to override (or supply) them.
Runtime values win:

```sh
export OMP_DECK_GOOGLE_CLIENT_ID='1234-abc.apps.googleusercontent.com'
export OMP_DECK_GOOGLE_CLIENT_SECRET='GOCSPX-…'
cargo run -p omp-deck            # baked in and used at runtime
# or, with any build:
OMP_DECK_GOOGLE_CLIENT_ID=… OMP_DECK_GOOGLE_CLIENT_SECRET=… ./target/release/omp-deck
```

Local state:

- `~/.config/omp-deck/google-drive.json` (mode 0600) holds the refresh token, the email, the last
  backup and the synced-content fingerprint. Deleting it signs out locally. **Sign out** in the
  card also revokes the token at Google.
- `~/.local/share/omp-deck/restore-safety/<UTC timestamp>/` holds the pre-restore safety copies.
  OmpDeck keeps the last 5.

The tests (`cargo test -p deck-core gdrive`) run against a local mock of Google's endpoints and
need no credentials.
