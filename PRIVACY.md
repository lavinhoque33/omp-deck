# OmpDeck privacy policy

OmpDeck is a desktop app that runs entirely on your computer. It has no servers and collects no
analytics or telemetry.

## Google Drive backup

If you sign in with Google (Settings → Backup → Back up to Google Drive), OmpDeck requests:

- `drive.appdata`: read and write files in OmpDeck's own hidden app-data folder in your Google
  Drive. OmpDeck cannot see or change any other file in your Drive.
- `openid` and `email`: show which Google account is signed in.

OmpDeck uses this access only to upload, list, download and delete its own backups:

- **omp settings backup** (Settings → Backup): configuration, custom models and agents, other omp
  config files, and prompt history.
- **Project sync** (Settings → Sync), only for the folders you add there and only the parts you switch
  on: a project's `.omp` folder (which can contain project settings and secrets you keep there),
  that project's omp session transcripts, including their attachments, and omp's learned memory for
  that project.

Logins and API credentials of omp itself are never backed up. Backups go straight from your computer
to your Google Drive; they are not sent anywhere else, and nobody other than you can access them.

The sign-in token is stored only on your computer, in `~/.config/omp-deck/google-drive.json`
(readable only by your user). "Sign out" revokes it at Google and deletes the file. You can also
remove OmpDeck's access at any time at <https://myaccount.google.com/permissions>. Project backups can be
deleted permanently in OmpDeck (Settings → Sync → Manage cloud backups…); to delete all backups, open
Google Drive → Settings → Manage apps → OmpDeck → Delete hidden app data.

OmpDeck's use of information received from Google APIs adheres to the
[Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy),
including the Limited Use requirements.

## Server backup (FTP, FTPS, SFTP)

Instead of Google Drive you can point OmpDeck at your own FTP, FTPS or SFTP server (Settings → Backup →
Back up to). It then uploads, lists, downloads and deletes the same backups described above (omp settings
and, if you use Project sync, the parts of your project folders you switch on) in the remote folder you
choose, and touches nothing else on the server. Only one destination is active at a time.

- **Where the data goes**: straight from your computer to the server you entered, and nowhere else.
  OmpDeck has no servers of its own and never sees your backups or credentials.
- **Credentials**: the host, user name, password and, for SFTP key login, the path to your private key
  file and its key passphrase are stored only on your computer, in `backup-server.json` in OmpDeck's config
  folder (`~/.config/omp-deck/`, readable only by your user (mode 0600); on Windows the per-user
  `%APPDATA%\omp-deck\` folder). They are saved as plain text in that file, not in an OS keychain, and the
  key file itself is not copied. They are sent only to your server; anonymous FTP logins have no password.
- **Plain FTP is unencrypted**: the login and the backups (which can contain project secrets) travel in the
  clear and can be read by anyone on the network path. OmpDeck warns about this; use SFTP or FTPS instead.
- **Pinned fingerprints**: when you trust an SFTP host key (always asked the first time) or an FTPS
  certificate that public root certificates do not vouch for, OmpDeck stores its SHA-256 fingerprint in the
  same file and refuses the server if the key or certificate ever differs.
- **Remove server** deletes `backup-server.json` and `project-sync-server.json` (the record of the last
  project backups on the server) from your computer. It deletes nothing on the server; the backup files stay
  there until you delete them (OmpDeck's **Manage backups…** dialogs delete individual backups).

## Contact

Questions: open an issue at <https://github.com/lavinhoque33/omp-deck/issues>.
